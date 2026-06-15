//! Application-owned install orchestration and journal lifecycle helpers.
//!
//! This module owns install command identity, worker coordination, journal
//! records, progress redaction, loader install coordination, and Guardian
//! artifact repair invocation. Core Minecraft code still owns provider
//! resolution, download verification, and concrete install effects.

use super::{
    ApplicationCommand, ApplicationCommandRequest, CommandResult, CommandResultCarriers,
    InstallVersionCommand, InstallVersionPayload, OperationCommandCarrier,
};
use crate::dto::loaders::{
    LoaderBuildsResponse, LoaderComponentsResponse, LoaderGameVersionsResponse,
};
use crate::guardian::{
    ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind, GuardianActionPlan,
    GuardianArtifactRepairOutcome, GuardianArtifactRepairRequest, GuardianArtifactRepairStatus,
    GuardianConfidence, GuardianDecision, GuardianDecisionKind,
    GuardianMinecraftArtifactRepairDescriptor, GuardianMode, GuardianRepairPlanningContext,
    diagnose_facts, execute_guardian_artifact_repair, execute_guardian_missing_artifact_repair,
    install_artifact_failure_from_minecraft_download_fact, install_artifact_failure_guardian_fact,
    plan_launcher_managed_artifact_repair, plan_launcher_managed_missing_artifact_repair,
};
use crate::install_runtime::prewarm_version_runtime;
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::{AppState, GuardianFailureMemoryStore, InstallStore, OperationJournalStore};
use axum::{Json, http::StatusCode};
use croopor_minecraft::download::{
    ExecutionDownloadFact, ExecutionDownloadFactKind, SelectedDownloadArtifactDescriptor,
};
use croopor_minecraft::{
    DownloadProgress, Downloader, LoaderComponentId, LoaderError, fetch_builds, fetch_components,
    fetch_supported_versions, install_build, resolve_build_record,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::SystemTime;
use tokio::sync::mpsc;

pub(crate) const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";
pub(crate) const LOADER_INSTALL_INTERRUPTED_MESSAGE: &str =
    "Loader install stopped before completing. Try again.";
pub(crate) const BASE_INSTALL_FAILED_MESSAGE: &str =
    "Base game install failed. Retry the install from Downloads.";

const LOADER_INSTALL_SCOPE: &str = "loader";
const VANILLA_INSTALL_SCOPE: &str = "vanilla";
const LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS: &str = "launcher_managed_artifact_corrupt";
const REPAIR_OPERATION_FACT_PREFIX: &str = "guardian_repair_operation:";
const REPAIR_STATUS_FACT_PREFIX: &str = "guardian_repair_status:";
const REPAIR_SUMMARY_FACT_PREFIX: &str = "guardian_repair_summary:";

pub type InstallApplicationError = (StatusCode, Json<serde_json::Value>);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionStaging {
    pub command: ApplicationCommand,
    pub result: CommandResult<InstallVersionPayload>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallGuardianRepairSummary {
    pub repair_operation_id: OperationId,
    pub diagnosis_id: String,
    pub status: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionStartRequest {
    pub version_id: String,
    #[serde(default)]
    pub manifest_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoaderInstallStartRequest {
    pub component_id: LoaderComponentId,
    pub build_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoaderBuildsRequest {
    pub component_id: LoaderComponentId,
    pub mc_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallStartResponse {
    pub install_id: String,
    pub operation_id: OperationId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallStatusResponse {
    pub install_id: String,
    pub operation_id: OperationId,
    pub done: bool,
    pub progress: Vec<DownloadProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_repair: Option<InstallGuardianRepairSummary>,
}

pub async fn start_install_version(
    state: &AppState,
    request: InstallVersionStartRequest,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let (version_id, manifest_url) = effective_install_fields(&request);
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

    let install_id = generate_install_id("install");
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
        return Ok(InstallStartResponse {
            install_id,
            operation_id,
        });
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

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
    })
}

pub async fn start_loader_install(
    state: &AppState,
    request: LoaderInstallStartRequest,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let build_id = request.build_id.trim().to_string();
    if build_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "build_id is required" })),
        ));
    }

    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir_path = PathBuf::from(&library_dir);

    let build = resolve_build_record(library_dir_path.as_path(), request.component_id, &build_id)
        .await
        .map_err(loader_error_response)?;

    let (install_version_key, install_manifest_key) =
        loader_install_key_fields(build.component_id, &build.build_id, &build.version_id);
    let target_version_id = build.version_id.clone();
    let install_id = generate_install_id("loader-install");
    let (install_id, inserted) = state
        .installs()
        .insert_or_existing_active_scoped(
            LOADER_INSTALL_SCOPE.to_string(),
            install_id,
            install_version_key,
            install_manifest_key,
        )
        .await;
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: target_version_id.clone(),
            manifest_url: None,
        },
        install_id.clone(),
        operation_id.clone(),
    );
    if !inserted {
        return Ok(InstallStartResponse {
            install_id,
            operation_id,
        });
    }
    begin_install_operation_journal(state.journals(), &operation_id, &target_version_id);

    let store = state.installs().clone();
    let journals = state.journals().clone();
    let library_dir = PathBuf::from(library_dir);
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id_task.clone();
    InstallStore::spawn_tracked_worker_with_interrupt_handler(
        store,
        install_id_task,
        interrupted_loader_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
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

            let version_id = build.version_id.clone();
            if let Err(progress) = wait_for_active_vanilla_base_install(
                &worker_store,
                &build.minecraft_version,
                &progress_tx,
            )
            .await
            {
                let _ = progress_tx.send(progress);
                drop(progress_tx);
                let _ = store_task.await;
                return;
            }

            let mut final_progress: Option<DownloadProgress> = None;
            let result = install_build(&library_dir, build, |progress| {
                if progress.done && progress.phase == "done" {
                    final_progress = Some(progress);
                } else {
                    let _ = progress_tx.send(progress);
                }
            })
            .await;

            if let Err(error) = result {
                let _ = progress_tx.send(loader_error_progress(error));
            } else if prewarm_version_runtime(&library_dir, &version_id, |progress| {
                let _ = progress_tx.send(progress);
            })
            .await
            .is_err()
            {
                let _ = progress_tx.send(prewarm_runtime_error_progress());
            } else if let Some(progress) = final_progress {
                let _ = progress_tx.send(progress);
            } else {
                let _ = progress_tx.send(loader_install_done_progress());
            }

            drop(progress_tx);
            let _ = store_task.await;
        },
        move |progress| {
            record_install_operation_interrupted(journals.as_ref(), &operation_id_task, &progress);
        },
    );

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
    })
}

pub fn loader_components() -> LoaderComponentsResponse {
    LoaderComponentsResponse {
        components: fetch_components(),
    }
}

pub async fn loader_builds(
    state: &AppState,
    request: LoaderBuildsRequest,
) -> Result<LoaderBuildsResponse, InstallApplicationError> {
    if request.mc_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "mc_version query parameter is required" })),
        ));
    }
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    fetch_builds(
        PathBuf::from(library_dir).as_path(),
        request.component_id,
        &request.mc_version,
    )
    .await
    .map(|(builds, catalog)| LoaderBuildsResponse { builds, catalog })
    .map_err(loader_error_response)
}

pub async fn loader_game_versions(
    state: &AppState,
    component_id: LoaderComponentId,
) -> Result<LoaderGameVersionsResponse, InstallApplicationError> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;

    fetch_supported_versions(PathBuf::from(library_dir).as_path(), component_id)
        .await
        .map(|(versions, catalog)| LoaderGameVersionsResponse { versions, catalog })
        .map_err(loader_error_response)
}

pub async fn install_status(
    state: &AppState,
    id: &str,
) -> Result<InstallStatusResponse, InstallApplicationError> {
    let operation_id = install_operation_id(id);
    let snapshot = state.installs().snapshot(id).await;
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

    Ok(InstallStatusResponse {
        install_id: public_install_id(id),
        operation_id,
        done,
        progress,
        guardian_repair,
    })
}

pub fn stage_install_version_command(
    request: InstallVersionCommand,
    install_id: String,
    operation_id: OperationId,
) -> InstallVersionStaging {
    let command = ApplicationCommandRequest::InstallVersion(request).command();
    let result = CommandResult {
        command: CommandKind::InstallVersion,
        operation_id: Some(operation_id.clone()),
        status: OperationStatus::Planned,
        safety: None,
        carriers: CommandResultCarriers {
            operation: Some(OperationCommandCarrier {
                operation_id: Some(operation_id.clone()),
                status: Some(OperationStatus::Planned),
                journal: None,
                events: Vec::new(),
                evidence: Vec::new(),
            }),
            ..CommandResultCarriers::default()
        },
        payload: InstallVersionPayload {
            install_id: Some(install_id),
            operation_id: Some(operation_id),
        },
        view_model: None,
    };

    InstallVersionStaging { command, result }
}

pub fn install_operation_id(install_id: &str) -> OperationId {
    let install_id = sanitize_evidence_token(install_id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install".to_string());
    OperationId::new(format!("install-operation-{install_id}"))
}

pub fn begin_install_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    version_id: &str,
) {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::InstallVersion,
        StabilizationSystem::Application,
        OwnershipClass::LauncherManaged,
        RollbackState::NotApplicable,
    );
    entry.targets.push(install_version_target(version_id));
    entry.planned_steps.push(install_journal_step(
        "install_version",
        OperationPhase::Planning,
        OperationStepResult::Planned,
        None,
    ));
    journals.create(entry);
}

pub fn record_install_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    last_recorded_phase: &mut Option<String>,
) {
    let phase = safe_progress_phase(&progress.phase);
    let terminal = progress.done;
    if !terminal && last_recorded_phase.as_deref() == Some(phase.as_str()) {
        return;
    }
    *last_recorded_phase = Some(phase.clone());

    if terminal && progress.error.is_some() {
        journals.record_failure(
            operation_id,
            install_progress_step(&phase, OperationStepResult::Failed, progress),
            format!("install_progress_{phase}"),
            OperationOutcome::Failed,
        );
        return;
    }

    if terminal {
        journals.record_success(
            operation_id,
            install_progress_step(&phase, OperationStepResult::Completed, progress),
            OperationOutcome::Succeeded,
        );
        return;
    }

    journals.record_progress(
        operation_id,
        install_progress_step(&phase, OperationStepResult::Completed, progress),
    );
}

pub fn record_install_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
) {
    let phase = safe_progress_phase(&progress.phase);
    journals.record_failure(
        operation_id,
        install_progress_step(&phase, OperationStepResult::Failed, progress),
        "install_worker_interrupted",
        OperationOutcome::Failed,
    );
}

pub fn record_install_operation_guardian_evidence(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) {
    let guardian_facts = facts
        .iter()
        .filter_map(|fact| {
            install_artifact_failure_from_minecraft_download_fact(
                Some(operation_id.clone()),
                OwnershipClass::LauncherManaged,
                fact,
            )
        })
        .map(|evidence| {
            install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading)
        })
        .collect::<Vec<_>>();
    if guardian_facts.is_empty() {
        return;
    }

    let fact_ids = guardian_facts
        .iter()
        .map(|fact| format!("guardian_fact:{}", fact.id.as_str()))
        .collect::<Vec<_>>();
    let diagnosis_ids = diagnose_facts(&guardian_facts, OperationPhase::Downloading)
        .into_iter()
        .map(|diagnosis| diagnosis.id.as_str().to_string())
        .collect::<Vec<_>>();
    journals.record_guardian_evidence(operation_id, fact_ids, diagnosis_ids);
}

pub fn record_install_operation_guardian_repair_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    outcome: &GuardianArtifactRepairOutcome,
) {
    let repair_operation_id = sanitize_evidence_token(
        outcome.operation_id.as_str(),
        RedactionAudience::UserVisible,
        96,
    )
    .unwrap_or_else(|| "guardian-repair".to_string());
    let diagnosis_id = sanitize_evidence_token(
        outcome.diagnosis_id.as_str(),
        RedactionAudience::UserVisible,
        96,
    )
    .unwrap_or_else(|| LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS.to_string());
    let summary = sanitize_evidence_token(&outcome.summary, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "guardian_artifact_repair".to_string());

    journals.record_guardian_evidence(
        operation_id,
        vec![
            format!("{REPAIR_OPERATION_FACT_PREFIX}{repair_operation_id}"),
            format!(
                "{REPAIR_STATUS_FACT_PREFIX}{}",
                guardian_artifact_repair_status_id(outcome.status)
            ),
            format!("{REPAIR_SUMMARY_FACT_PREFIX}{summary}"),
        ],
        vec![diagnosis_id],
    );
}

pub fn install_guardian_repair_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<InstallGuardianRepairSummary> {
    let repair_operation_id = latest_generated_fact_value(entry, REPAIR_OPERATION_FACT_PREFIX)?;
    let status = latest_generated_fact_value(entry, REPAIR_STATUS_FACT_PREFIX)?;
    let diagnosis_id = entry
        .guardian_diagnosis_ids
        .iter()
        .rev()
        .find(|diagnosis_id| diagnosis_id.as_str() == LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS)
        .cloned()
        .unwrap_or_else(|| LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS.to_string());
    let summary = latest_generated_fact_value(entry, REPAIR_SUMMARY_FACT_PREFIX)
        .unwrap_or_else(|| "guardian_artifact_repair".to_string());
    let (label, detail) = install_repair_summary_copy(&status, &summary);

    Some(InstallGuardianRepairSummary {
        repair_operation_id: OperationId::new(repair_operation_id),
        diagnosis_id,
        status,
        label: label.to_string(),
        detail: detail.map(str::to_string),
    })
}

pub async fn repair_install_artifact_corruption_with_guardian(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    client: &Client,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
    descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    let descriptor = first_repairable_install_artifact_descriptor(facts, descriptors)?;
    let decision = install_artifact_repair_decision(operation_id, descriptor.target().clone());
    let destination_missing = descriptor
        .destination()
        .try_exists()
        .is_ok_and(|exists| !exists);
    let plan = if destination_missing {
        plan_launcher_managed_missing_artifact_repair(
            &decision,
            GuardianRepairPlanningContext::default(),
        )
        .ok()?
    } else {
        plan_launcher_managed_artifact_repair(&decision, GuardianRepairPlanningContext::default())
            .ok()?
    };

    let request = GuardianArtifactRepairRequest {
        operation_id: None,
        plan: &plan,
        destination: descriptor.destination(),
        source: descriptor.repair_source(),
        client,
        journals,
        failure_memory,
        mode: GuardianMode::Managed,
        observed_at,
    };

    if destination_missing {
        Some(execute_guardian_missing_artifact_repair(request).await)
    } else {
        Some(execute_guardian_artifact_repair(request).await)
    }
}

pub(crate) fn effective_install_fields(request: &InstallVersionStartRequest) -> (String, String) {
    (
        request.version_id.trim().to_string(),
        request.manifest_url.trim().to_string(),
    )
}

pub fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
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

pub(crate) fn interrupted_install_progress() -> DownloadProgress {
    observed_install_failure_progress()
}

pub(crate) fn observed_install_failure_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(INSTALL_FAILURE_MESSAGE.to_string()),
        done: true,
    }
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

pub(crate) fn loader_install_key_fields(
    component_id: LoaderComponentId,
    build_id: &str,
    version_id: &str,
) -> (String, String) {
    (
        format!("loader:{}:{}", component_id.as_str(), version_id.trim()),
        format!("loader:{}:{}", component_id.as_str(), build_id.trim()),
    )
}

pub(crate) async fn wait_for_active_vanilla_base_install(
    store: &InstallStore,
    version_id: &str,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
) -> Result<(), DownloadProgress> {
    let Some(install_id) = store
        .active_install_for_scope_and_version(VANILLA_INSTALL_SCOPE, version_id)
        .await
    else {
        return Ok(());
    };

    let Some((history, mut receiver, done)) = store.subscribe(&install_id).await else {
        return Ok(());
    };

    for progress in history {
        if progress.done {
            return if progress.error.is_some() {
                Err(base_install_failed_progress())
            } else {
                Ok(())
            };
        }
        let _ = progress_tx.send(progress);
    }
    if done {
        return Ok(());
    }

    loop {
        match receiver.recv().await {
            Ok(progress) => {
                if progress.done {
                    return if progress.error.is_some() {
                        Err(base_install_failed_progress())
                    } else {
                        Ok(())
                    };
                }
                let _ = progress_tx.send(progress);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

pub fn loader_error_response(error: LoaderError) -> InstallApplicationError {
    let status = match error {
        LoaderError::InvalidMinecraftVersion
        | LoaderError::InvalidBuildId
        | LoaderError::InvalidComponentId => StatusCode::BAD_REQUEST,
        LoaderError::BuildNotFound(_) => StatusCode::NOT_FOUND,
        LoaderError::MissingLibraryDir => StatusCode::PRECONDITION_FAILED,
        LoaderError::CatalogUnavailable(_) | LoaderError::ArtifactMissing(_) => {
            StatusCode::BAD_GATEWAY
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({
            "error": public_loader_error_message(&error),
            "failure_kind": error.failure_kind(),
        })),
    )
}

pub(crate) fn loader_error_progress(error: LoaderError) -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(public_loader_error_message(&error).to_string()),
        done: true,
    }
}

pub(crate) fn prewarm_runtime_error_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(public_runtime_error_message().to_string()),
        done: true,
    }
}

pub(crate) fn base_install_failed_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(BASE_INSTALL_FAILED_MESSAGE.to_string()),
        done: true,
    }
}

pub(crate) fn loader_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
    }
}

pub(crate) fn interrupted_loader_install_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(LOADER_INSTALL_INTERRUPTED_MESSAGE.to_string()),
        done: true,
    }
}

fn public_loader_error_message(error: &LoaderError) -> &'static str {
    match error {
        LoaderError::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderError::InvalidBuildId => "Invalid loader build.",
        LoaderError::InvalidComponentId => "Invalid loader component.",
        LoaderError::MissingLibraryDir => "Croopor library is not configured",
        LoaderError::CatalogUnavailable(_) => {
            "Loader catalog is unavailable. Check your connection and try again."
        }
        LoaderError::BuildNotFound(_) => "Selected loader build is not available.",
        LoaderError::ArtifactMissing(_) => {
            "Loader artifact is unavailable. Try another build or component."
        }
        LoaderError::InvalidProfile(_) => "Loader profile is invalid. Try another build.",
        LoaderError::Verify(_) => {
            "Loader install verification failed. Try again or choose another build."
        }
        LoaderError::Request(_) => {
            "Loader service request failed. Check your connection and try again."
        }
        LoaderError::Download(_) => "Loader download failed. Check your connection and try again.",
        LoaderError::Parse(_) => "Loader service returned unreadable data. Try again later.",
        LoaderError::Io(_) => {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderError::Other(_) => "Loader operation failed. Try again.",
    }
}

fn public_runtime_error_message() -> &'static str {
    "Could not prepare the Java runtime. Check your connection and try again."
}

fn generate_install_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{:032x}", nanos)
}

fn install_progress_step(
    phase: &str,
    result: OperationStepResult,
    progress: &DownloadProgress,
) -> OperationJournalStep {
    let mut step = install_journal_step(
        format!("install_progress_{phase}"),
        install_operation_phase(progress),
        result,
        None,
    );
    step.generated_facts.push(format!("install_phase:{phase}"));
    if progress.done {
        step.generated_facts.push("install_done:true".to_string());
    }
    if progress.error.is_some() {
        step.generated_facts.push("install_error:true".to_string());
    }
    step
}

fn install_journal_step(
    step_id: impl AsRef<str>,
    phase: OperationPhase,
    result: OperationStepResult,
    changed_target: Option<TargetDescriptor>,
) -> OperationJournalStep {
    let step_id = sanitize_evidence_token(step_id.as_ref(), RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install_step".to_string());
    let mut step = OperationJournalStep::new(step_id, phase);
    step.result = result;
    step.changed_target = changed_target;
    step.rollback = RollbackState::NotApplicable;
    step
}

fn install_operation_phase(progress: &DownloadProgress) -> OperationPhase {
    if progress.done && progress.error.is_some() {
        return OperationPhase::Failed;
    }
    if progress.done {
        return OperationPhase::Completed;
    }

    match progress.phase.trim() {
        "version_json" | "client_jar" | "libraries" | "asset_index" | "assets" | "log_config"
        | "java_runtime" | "loader_meta" | "loader_json" | "artifacts" | "loader_libraries" => {
            OperationPhase::Downloading
        }
        "profile" | "loader_processors" | "processors" => OperationPhase::Installing,
        _ => OperationPhase::Running,
    }
}

fn install_version_target(version_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Version,
        version_id,
        OwnershipClass::LauncherManaged,
    )
}

fn safe_progress_phase(phase: &str) -> String {
    sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string())
}

fn latest_generated_fact_value(entry: &OperationJournalEntry, prefix: &str) -> Option<String> {
    entry
        .completed_steps
        .iter()
        .rev()
        .flat_map(|step| step.generated_facts.iter().rev())
        .find_map(|fact| fact.strip_prefix(prefix).map(str::to_string))
}

fn guardian_artifact_repair_status_id(status: GuardianArtifactRepairStatus) -> &'static str {
    match status {
        GuardianArtifactRepairStatus::Repaired => "repaired",
        GuardianArtifactRepairStatus::Blocked => "blocked",
        GuardianArtifactRepairStatus::Failed => "failed",
        GuardianArtifactRepairStatus::Suppressed => "suppressed",
    }
}

fn install_repair_summary_copy(
    status: &str,
    _summary: &str,
) -> (&'static str, Option<&'static str>) {
    match status {
        "repaired" => (
            "Guardian repaired a launcher-managed install artifact.",
            Some("Retry the install to continue from the repaired state."),
        ),
        "suppressed" => (
            "Guardian paused automatic install repair after repeated failure.",
            Some("Check connection and storage permissions before trying again."),
        ),
        "blocked" => (
            "Guardian blocked automatic install repair because it was unsafe.",
            Some("The launcher did not mutate files that were not proven launcher-managed."),
        ),
        "failed" => (
            "Guardian could not repair the launcher-managed install artifact.",
            Some("Check connection and storage permissions before trying again."),
        ),
        _ => (
            "Guardian recorded an install repair outcome.",
            Some("Check the install operation status before retrying."),
        ),
    }
}

fn first_repairable_install_artifact_descriptor<'a>(
    facts: &[ExecutionDownloadFact],
    descriptors: &'a [SelectedDownloadArtifactDescriptor],
) -> Option<GuardianMinecraftArtifactRepairDescriptor> {
    facts
        .iter()
        .filter(|fact| repairable_install_artifact_fact_kind(fact.kind))
        .filter_map(|fact| {
            descriptors
                .iter()
                .find(|descriptor| descriptor.target == fact.target)
        })
        .find_map(|descriptor| {
            GuardianMinecraftArtifactRepairDescriptor::from_core_selected_descriptor(descriptor)
                .ok()
        })
}

fn repairable_install_artifact_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ChecksumMismatch | ExecutionDownloadFactKind::SizeMismatch
    )
}

fn install_artifact_repair_decision(
    operation_id: &OperationId,
    target: TargetDescriptor,
) -> GuardianDecision {
    let diagnosis_id = DiagnosisId::new(LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS);
    GuardianDecision {
        operation_id: Some(operation_id.clone()),
        mode: GuardianMode::Managed,
        kind: GuardianDecisionKind::Repair,
        diagnoses: vec![diagnosis_id.clone()],
        action_plan: Some(GuardianActionPlan::new(
            StabilizationSystem::Guardian,
            ActionPlanPrerequisite {
                diagnosis_id: diagnosis_id.clone(),
                ownership: OwnershipClass::LauncherManaged,
                confidence: GuardianConfidence::Confirmed,
                affected_targets: vec![target.clone()],
                candidate_actions: vec![
                    GuardianActionKind::Quarantine,
                    GuardianActionKind::Repair,
                    GuardianActionKind::Block,
                ],
            },
            vec![GuardianAction {
                kind: GuardianActionKind::Repair,
                target: Some(target),
                reason: diagnosis_id,
            }],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        begin_install_operation_journal, install_guardian_repair_summary_from_journal,
        install_operation_id, record_install_operation_guardian_evidence,
        record_install_operation_guardian_repair_outcome, record_install_operation_interrupted,
        record_install_operation_progress, repair_install_artifact_corruption_with_guardian,
        stage_install_version_command,
    };
    use crate::application::InstallVersionCommand;
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianArtifactRepairOutcome,
        GuardianArtifactRepairStatus,
    };
    use crate::state::contracts::{
        CommandKind, OperationId, OperationOutcome, OperationStatus, OperationStepResult,
        TargetKind,
    };
    use crate::state::{GuardianFailureMemoryStore, OperationJournalStore};
    use croopor_minecraft::DownloadProgress;
    use croopor_minecraft::download::{
        ExecutionDownloadFact, ExecutionDownloadFactKind, SelectedDownloadArtifactDescriptor,
        SelectedDownloadArtifactKind,
    };
    use sha1::{Digest, Sha1};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;
    use std::{fs, sync::mpsc};

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
    fn install_journal_records_progress_success_and_redacts_fields() {
        let journals = OperationJournalStore::new();
        let operation_id = install_operation_id(r"C:\Users\Alice\token-install");
        begin_install_operation_journal(
            &journals,
            &operation_id,
            r"C:\Users\Alice\.minecraft\versions\secret.jar",
        );

        let mut last_phase = None;
        record_install_operation_progress(
            &journals,
            &operation_id,
            &progress("libraries", false, None),
            &mut last_phase,
        );
        record_install_operation_progress(
            &journals,
            &operation_id,
            &progress("libraries", false, None),
            &mut last_phase,
        );
        record_install_operation_progress(
            &journals,
            &operation_id,
            &progress("done", true, None),
            &mut last_phase,
        );

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

    #[test]
    fn install_journal_records_failure_and_interruption() {
        let journals = OperationJournalStore::new();
        let failed_operation = install_operation_id("install-failed");
        begin_install_operation_journal(&journals, &failed_operation, "1.21.5");
        let mut last_phase = None;
        record_install_operation_progress(
            &journals,
            &failed_operation,
            &progress(
                r"C:\Users\Alice\.minecraft -Xmx8192M --accessToken provider_payload",
                true,
                Some(
                    "failed in /Users/alice/.croopor with token secret provider_payload={\"token\":\"secret\"}",
                ),
            ),
            &mut last_phase,
        );
        let failed = journals.get(&failed_operation).expect("failed journal");
        assert_eq!(failed.status, OperationStatus::Failed);
        assert_eq!(failed.outcome, Some(OperationOutcome::Failed));
        assert_no_sensitive_fragments(&serde_json::to_string(&failed).expect("journal json"));

        let interrupted_operation = install_operation_id("install-interrupted");
        begin_install_operation_journal(&journals, &interrupted_operation, "1.21.5");
        record_install_operation_interrupted(
            &journals,
            &interrupted_operation,
            &progress("error", true, Some("worker interrupted")),
        );
        let interrupted = journals
            .get(&interrupted_operation)
            .expect("interrupted journal");
        assert_eq!(interrupted.status, OperationStatus::Failed);
        assert_eq!(
            interrupted.failure_point.as_deref(),
            Some("install_worker_interrupted")
        );
    }

    #[test]
    fn install_journal_records_guardian_evidence_from_core_download_facts() {
        let journals = OperationJournalStore::new();
        let operation_id = install_operation_id("install-guardian-evidence");
        begin_install_operation_journal(&journals, &operation_id, "1.21.5");
        let mut last_phase = None;
        record_install_operation_progress(
            &journals,
            &operation_id,
            &progress("error", true, Some("sanitized failure")),
            &mut last_phase,
        );

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
        );

        let entry = journals.get(&operation_id).expect("journal");
        assert_eq!(entry.status, OperationStatus::Failed);
        assert_eq!(
            entry.guardian_diagnosis_ids,
            vec!["launcher_managed_artifact_corrupt".to_string()]
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

    #[test]
    fn install_journal_records_guardian_repair_summary_without_raw_details() {
        let journals = OperationJournalStore::new();
        let operation_id = install_operation_id("install-guardian-repair-summary");
        begin_install_operation_journal(&journals, &operation_id, "1.21.5");
        let mut last_phase = None;
        record_install_operation_progress(
            &journals,
            &operation_id,
            &progress("error", true, Some("sanitized failure")),
            &mut last_phase,
        );

        record_install_operation_guardian_repair_outcome(
            &journals,
            &operation_id,
            &GuardianArtifactRepairOutcome {
                operation_id: OperationId::new(
                    "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000",
                ),
                diagnosis_id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                action: GuardianActionKind::Repair,
                status: GuardianArtifactRepairStatus::Suppressed,
                facts: vec!["https://example.invalid/artifact.jar?token=secret".to_string()],
                summary: "guardian_artifact_repair_suppressed".to_string(),
            },
        );

        let entry = journals.get(&operation_id).expect("journal");
        let summary = install_guardian_repair_summary_from_journal(&entry).expect("repair summary");
        assert_eq!(summary.status, "suppressed");
        assert_eq!(
            summary.repair_operation_id.as_str(),
            "guardian-artifact-repair:123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            summary.diagnosis_id,
            "launcher_managed_artifact_corrupt".to_string()
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
        let facts = vec![download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            "client.jar",
        )];
        let descriptors = vec![selected_descriptor(
            SelectedDownloadArtifactKind::ClientJar,
            "client.jar",
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
        assert_no_sensitive_fragments(
            &serde_json::to_string(&repair_journal).expect("journal json"),
        );

        server.stop();
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
        let facts = vec![download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            "missing-client.jar",
        )];
        let descriptors = vec![selected_descriptor(
            SelectedDownloadArtifactKind::ClientJar,
            "missing-client.jar",
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
        .await;
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
        .await;

        assert!(network_outcome.is_none());
        assert!(unmatched_outcome.is_none());
        assert_eq!(fs::read(&destination).expect("artifact"), b"corrupt client");
        assert!(failure_memory.list().is_empty());

        let _ = fs::remove_dir_all(root);
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
            file: Some("/Users/alice/.croopor/libraries/secret.jar".to_string()),
            error: error.map(str::to_string),
            done,
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

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-install-application-{name}-{}-{}",
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
}
