use super::{
    INSTALL_FAILURE_MESSAGE, InstallGuardianOutcomeSummary, InstallProgressStepViewModel,
    InstallProgressViewModel, InstallVersionStaging,
};
use crate::application::{
    ApplicationCommandRequest, CommandResult, CommandResultCarriers, InstallVersionCommand,
    InstallVersionPayload, OperationCommandCarrier,
};
use crate::guardian::{
    GuardianDecisionKind, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianMode, GuardianPolicyContext, diagnose_facts,
    install_artifact_failure_from_minecraft_download_fact, install_artifact_failure_guardian_fact,
    install_artifact_failure_guardian_outcome_with_context, install_artifact_failure_safety_case,
};
use crate::observability::{
    RedactionAudience, sanitize_evidence_text, sanitize_evidence_token,
    sanitize_public_diagnostic_text,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
};
use crate::state::{GuardianFailureMemoryStore, OperationJournalStore};
use croopor_minecraft::DownloadProgress;
use croopor_minecraft::download::ExecutionDownloadFact;
use croopor_minecraft::{LoaderError, LoaderInstallFailureKind};
use serde_json::{Value, json};

const PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES: i64 = 5;
const PROVIDER_FAILURE_MEMORY_SOURCE: &str = "install_provider";

const GUARDIAN_OUTCOME_DECISION_PREFIX: &str = "guardian_outcome_decision:";
const GUARDIAN_OUTCOME_SUMMARY_PREFIX: &str = "guardian_outcome_summary:";
const GUARDIAN_OUTCOME_DETAIL_PREFIX: &str = "guardian_outcome_detail:";
const GUARDIAN_OUTCOME_GUIDANCE_PREFIX: &str = "guardian_outcome_guidance:";

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
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        "install_worker_interrupted",
        GuardianInstallArtifactFailureKind::NetworkFailure,
    )
    .with_field("phase", phase);
    record_install_operation_guardian_failure_outcome_from_evidence(
        journals,
        operation_id,
        &[evidence],
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

pub fn record_install_operation_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) {
    let evidence = install_failure_evidence_from_download_facts(operation_id, facts);
    record_install_operation_guardian_failure_outcome_from_evidence(
        journals,
        operation_id,
        &evidence,
    );
}

pub fn record_install_operation_guardian_failure_outcome_with_memory(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
    observed_at: &str,
) {
    let evidence = install_failure_evidence_from_download_facts(operation_id, facts);
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        Some(failure_memory),
        operation_id,
        &evidence,
        observed_at,
    );
}

pub fn record_loader_install_operation_guardian_failure_outcome(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    operation_id: &OperationId,
    target_id: &str,
    error: &LoaderError,
    observed_at: &str,
) {
    let Some(evidence) = loader_error_guardian_failure_evidence(operation_id, target_id, error)
    else {
        return;
    };
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        Some(failure_memory),
        operation_id,
        &[evidence],
        observed_at,
    );
}

pub fn record_loader_base_install_dependency_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target_id: &str,
    base_version_id: &str,
) {
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        GuardianInstallArtifactFailureKind::DependencyFailed,
    )
    .with_field("dependency", "base_version")
    .with_field("base_version", base_version_id);
    record_install_operation_guardian_failure_outcome_from_evidence(
        journals,
        operation_id,
        &[evidence],
    );
}

pub fn install_guardian_outcome_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<InstallGuardianOutcomeSummary> {
    let decision = latest_generated_fact_value(entry, GUARDIAN_OUTCOME_DECISION_PREFIX)?;
    let label = latest_generated_fact_value(entry, GUARDIAN_OUTCOME_SUMMARY_PREFIX)?;
    let diagnosis_id =
        entry.guardian_diagnosis_ids.iter().rev().find_map(|id| {
            (id.as_str() != "launcher_managed_artifact_corrupt").then(|| id.clone())
        })?;
    let detail = latest_generated_fact_value(entry, GUARDIAN_OUTCOME_DETAIL_PREFIX);
    let guidance = latest_generated_fact_value(entry, GUARDIAN_OUTCOME_GUIDANCE_PREFIX)
        .into_iter()
        .collect();

    Some(InstallGuardianOutcomeSummary {
        diagnosis_id,
        decision,
        label,
        detail,
        guidance,
    })
}

pub fn sanitize_install_progress(mut progress: DownloadProgress) -> DownloadProgress {
    progress.phase = sanitize_evidence_token(&progress.phase, RedactionAudience::UserVisible, 48)
        .unwrap_or_else(|| "install".to_string());
    progress.file = progress
        .file
        .take()
        .and_then(|file| sanitize_evidence_token(&file, RedactionAudience::UserVisible, 96));
    progress.error = progress.error.take().map(|error| {
        if progress.done {
            return INSTALL_FAILURE_MESSAGE.to_string();
        }
        sanitize_public_diagnostic_text(
            &error,
            RedactionAudience::UserVisible,
            160,
            INSTALL_FAILURE_MESSAGE,
        )
    });
    progress
}

pub fn vanilla_install_progress_view_model(
    progress: &DownloadProgress,
) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Vanilla)
}

pub fn loader_install_progress_view_model(progress: &DownloadProgress) -> InstallProgressViewModel {
    install_progress_view_model(progress, InstallProgressKind::Loader)
}

pub fn public_vanilla_install_progress_json(progress: &DownloadProgress) -> Value {
    public_install_progress_json(progress, InstallProgressKind::Vanilla)
}

pub fn public_loader_install_progress_json(progress: &DownloadProgress) -> Value {
    public_install_progress_json(progress, InstallProgressKind::Loader)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallProgressKind {
    Vanilla,
    Loader,
}

fn public_install_progress_json(progress: &DownloadProgress, kind: InstallProgressKind) -> Value {
    let progress = sanitize_install_progress(progress.clone());
    let mut payload = serde_json::to_value(&progress).unwrap_or_else(|_| json!({}));
    payload["view_model"] = json!(install_progress_view_model(&progress, kind));
    payload
}

fn install_progress_view_model(
    progress: &DownloadProgress,
    kind: InstallProgressKind,
) -> InstallProgressViewModel {
    let progress = sanitize_install_progress(progress.clone());
    let phase = progress.phase.trim();
    let label = install_progress_label(&progress, kind);
    let failed = phase == "error" || progress.error.is_some();
    let terminal = progress.done || failed;
    InstallProgressViewModel {
        phase_id: if phase.is_empty() {
            "install".to_string()
        } else {
            phase.to_string()
        },
        progress_pct: install_progress_pct(&progress, kind),
        active_step: install_active_step_view_model(&progress, &label),
        label,
        terminal,
        failed,
    }
}

fn install_progress_label(progress: &DownloadProgress, kind: InstallProgressKind) -> String {
    match progress.phase.as_str() {
        "loader_meta" => "Fetching loader info".to_string(),
        "loader_json" => "Preparing loader".to_string(),
        "profile" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Preparing loader profile".to_string()),
        "artifacts" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Downloading loader artifacts".to_string()),
        "loader_libraries" => count_label("Loader libraries", progress),
        "loader_processors" | "processors" => progress
            .file
            .clone()
            .unwrap_or_else(|| count_label("Running processors", progress)),
        "version_json" => "Fetching version info".to_string(),
        "client_jar" => "Downloading game JAR".to_string(),
        "libraries" => count_label("Libraries", progress),
        "asset_index" => "Downloading asset index".to_string(),
        "assets" => count_label("Assets", progress),
        "log_config" => "Downloading log config".to_string(),
        "java_runtime" => progress
            .file
            .clone()
            .unwrap_or_else(|| "Preparing Java runtime".to_string()),
        "done" => "Complete".to_string(),
        "error" => progress
            .error
            .clone()
            .unwrap_or_else(|| INSTALL_FAILURE_MESSAGE.to_string()),
        phase => progress.file.clone().unwrap_or_else(|| match kind {
            InstallProgressKind::Loader => {
                if phase.is_empty() {
                    "Working on loader install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
            InstallProgressKind::Vanilla => {
                if phase.is_empty() {
                    "Working on install".to_string()
                } else {
                    format!("Working on {phase}")
                }
            }
        }),
    }
}

fn install_progress_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> u8 {
    let pct = match (kind, progress.phase.as_str()) {
        (_, "done") => 100,
        (_, "error") => 100,
        (InstallProgressKind::Vanilla, "version_json") => 2,
        (InstallProgressKind::Vanilla, "client_jar") => 7,
        (InstallProgressKind::Vanilla, "libraries") => {
            7 + (progress_fraction(progress) * 13.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "asset_index") => 21,
        (InstallProgressKind::Vanilla, "assets") => {
            21 + (progress_fraction(progress) * 72.0).round() as i32
        }
        (InstallProgressKind::Vanilla, "log_config") => 94,
        (InstallProgressKind::Vanilla, "java_runtime") => {
            if is_java_runtime_ready_progress(progress) {
                95
            } else {
                94
            }
        }
        (InstallProgressKind::Loader, "loader_meta") => 1,
        (InstallProgressKind::Loader, "loader_json" | "profile") => 3,
        (InstallProgressKind::Loader, "artifacts") => 6,
        (InstallProgressKind::Loader, "loader_libraries") => {
            3 + (progress_fraction(progress) * 7.0).round() as i32
        }
        (InstallProgressKind::Loader, "loader_processors" | "processors") => {
            10 + (progress_fraction(progress) * 10.0).round() as i32
        }
        (InstallProgressKind::Loader, "version_json") => 21,
        (InstallProgressKind::Loader, "client_jar") => 24,
        (InstallProgressKind::Loader, "libraries") => {
            24 + (progress_fraction(progress) * 10.0).round() as i32
        }
        (InstallProgressKind::Loader, "asset_index") => 35,
        (InstallProgressKind::Loader, "assets") => {
            35 + (progress_fraction(progress) * 58.0).round() as i32
        }
        (InstallProgressKind::Loader, "log_config") => 94,
        (InstallProgressKind::Loader, "java_runtime") => {
            if is_java_runtime_ready_progress(progress) {
                95
            } else {
                94
            }
        }
        _ => 0,
    };
    pct.clamp(0, 100) as u8
}

fn install_active_step_view_model(
    progress: &DownloadProgress,
    label: &str,
) -> Option<InstallProgressStepViewModel> {
    if !matches!(
        progress.phase.as_str(),
        "java_runtime" | "loader_processors" | "processors"
    ) {
        return None;
    }
    if progress.total <= 0 {
        return None;
    }

    Some(InstallProgressStepViewModel {
        phase_id: progress.phase.clone(),
        label: label.to_string(),
        progress_pct: (progress_fraction(progress) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8,
        current: progress.current.max(0),
        total: progress.total,
    })
}

fn count_label(base: &str, progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        format!("{} ({}/{})", base, progress.current.max(0), progress.total)
    } else {
        base.to_string()
    }
}

fn progress_fraction(progress: &DownloadProgress) -> f32 {
    if progress.total <= 0 {
        return 0.0;
    }
    (progress.current.max(0) as f32 / progress.total as f32).clamp(0.0, 1.0)
}

fn is_java_runtime_ready_progress(progress: &DownloadProgress) -> bool {
    progress.phase == "java_runtime"
        && progress.current == 1
        && progress.total == 1
        && progress
            .file
            .as_ref()
            .is_some_and(|file| file.trim().to_ascii_lowercase().starts_with("ready "))
}

fn install_failure_evidence_from_download_facts(
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) -> Vec<GuardianInstallArtifactFailureEvidence> {
    facts
        .iter()
        .filter_map(|fact| {
            install_artifact_failure_from_minecraft_download_fact(
                Some(operation_id.clone()),
                OwnershipClass::LauncherManaged,
                fact,
            )
        })
        .collect()
}

fn record_install_operation_guardian_failure_outcome_from_evidence(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
) {
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        None,
        operation_id,
        evidence,
        &chrono::Utc::now().to_rfc3339(),
    );
}

fn record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
    journals: &OperationJournalStore,
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) {
    let mode = GuardianMode::Managed;
    let phase = OperationPhase::Downloading;
    let context = failure_memory_suppression_context(
        failure_memory,
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        observed_at,
    );
    let Some(outcome) = install_artifact_failure_guardian_outcome_with_context(
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        context,
    ) else {
        return;
    };

    record_provider_failure_memory_if_needed(
        failure_memory,
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        &outcome,
        observed_at,
    );

    let mut facts = vec![
        prefixed_token_fact(
            GUARDIAN_OUTCOME_DECISION_PREFIX,
            guardian_decision_kind_id(outcome.decision),
            "guardian_decision",
            48,
        ),
        prefixed_text_fact(
            GUARDIAN_OUTCOME_SUMMARY_PREFIX,
            &outcome.user_outcome.summary,
            "Guardian recorded an install safety outcome.",
            220,
        ),
    ];
    if let Some(detail) = outcome.user_outcome.details.first() {
        facts.push(prefixed_text_fact(
            GUARDIAN_OUTCOME_DETAIL_PREFIX,
            detail,
            "Guardian recorded bounded install failure details.",
            240,
        ));
    }
    if let Some(guidance) = outcome.user_outcome.guidance.first() {
        facts.push(prefixed_text_fact(
            GUARDIAN_OUTCOME_GUIDANCE_PREFIX,
            guidance,
            "Retry the install after checking connection and storage availability.",
            240,
        ));
    }

    journals.record_guardian_evidence(
        operation_id,
        facts,
        vec![outcome.diagnosis_id.as_str().to_string()],
    );
}

fn loader_error_guardian_failure_evidence(
    operation_id: &OperationId,
    target_id: &str,
    error: &LoaderError,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    let (kind, ownership) = match error.failure_kind() {
        LoaderInstallFailureKind::CatalogUnavailable
        | LoaderInstallFailureKind::CatalogStale
        | LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderRateLimited
        | LoaderInstallFailureKind::ArtifactMissing => (
            GuardianInstallArtifactFailureKind::ProviderFailure,
            OwnershipClass::ExternalProviderDerived,
        ),
        LoaderInstallFailureKind::ProviderNetworkFailure
        | LoaderInstallFailureKind::RequestFailed
        | LoaderInstallFailureKind::DownloadFailed => (
            GuardianInstallArtifactFailureKind::NetworkFailure,
            OwnershipClass::ExternalProviderDerived,
        ),
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid
        | LoaderInstallFailureKind::InvalidProfile
        | LoaderInstallFailureKind::ParseFailed
        | LoaderInstallFailureKind::VerifyFailed => (
            GuardianInstallArtifactFailureKind::MetadataInvalid,
            OwnershipClass::ExternalProviderDerived,
        ),
        LoaderInstallFailureKind::IoFailed => (
            GuardianInstallArtifactFailureKind::PermissionDenied,
            OwnershipClass::LauncherManaged,
        ),
        LoaderInstallFailureKind::BaseInstallFailed
        | LoaderInstallFailureKind::BuildNotFound
        | LoaderInstallFailureKind::ProcessorFailed
        | LoaderInstallFailureKind::Other => return None,
    };

    let mut evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        kind,
    )
    .with_ownership(ownership)
    .with_field("failure_kind", error.failure_kind().as_str());
    if let Some(provider_kind) = error.provider_failure_kind() {
        evidence = evidence.with_field("provider_failure", provider_kind.as_str());
    }
    if let Some(status) = error.provider_status() {
        evidence = evidence.with_field("status", status.to_string());
    }
    Some(evidence)
}

fn failure_memory_suppression_context(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) -> GuardianPolicyContext {
    let mut context = GuardianPolicyContext::current_operation();
    if provider_failure_memory_entry(
        failure_memory,
        operation_id,
        mode,
        phase,
        evidence,
        observed_at,
    )
    .is_some()
    {
        context = context.with_suppression();
    }
    context
}

fn provider_failure_memory_entry(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) -> Option<crate::state::failure_memory::GuardianFailureMemoryEntry> {
    let memory = failure_memory?;
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let diagnosis = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id.as_str() == "download_unavailable")?;
    let target = diagnosis.affected_targets.first()?;
    let key = FailureMemoryKey::for_observation(
        diagnosis.domain,
        &diagnosis.id,
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
    );
    let entry = memory.get(&key)?;
    if !suppression_active(&entry, observed_at) {
        return None;
    }
    Some(entry)
}

fn record_provider_failure_memory_if_needed(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    outcome: &crate::guardian::GuardianInstallFailureOutcome,
    observed_at: &str,
) {
    if outcome.diagnosis_id.as_str() != "download_unavailable" {
        return;
    }
    let Some(memory) = failure_memory else {
        return;
    };
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let Some(diagnosis) = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id.as_str() == outcome.diagnosis_id.as_str())
    else {
        return;
    };
    let Some(target) = diagnosis.affected_targets.first().cloned() else {
        return;
    };
    let suppression_until = provider_failure_suppression_until(observed_at);
    let entry = GuardianFailureMemoryEntry::observed(
        diagnosis.id.clone(),
        diagnosis.domain,
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
        observed_at.to_string(),
    )
    .with_action(
        guardian_action_kind_for_decision(outcome.decision),
        failure_memory_outcome_for_decision(outcome.decision),
    )
    .with_suppression_until(suppression_until);
    let _ = memory.record(entry);
}

fn provider_failure_suppression_until(observed_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(observed_at)
        .map(|timestamp| {
            (timestamp + chrono::Duration::minutes(PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES))
                .to_rfc3339()
        })
        .unwrap_or_else(|_| {
            (chrono::Utc::now()
                + chrono::Duration::minutes(PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES))
            .to_rfc3339()
        })
}

fn suppression_active(entry: &GuardianFailureMemoryEntry, observed_at: &str) -> bool {
    let Some(suppression_until) = &entry.suppression_until else {
        return false;
    };
    let Ok(suppression_until) = chrono::DateTime::parse_from_rfc3339(suppression_until) else {
        return false;
    };
    let observed_at = chrono::DateTime::parse_from_rfc3339(observed_at)
        .unwrap_or_else(|_| chrono::Utc::now().fixed_offset());
    suppression_until > observed_at
}

fn guardian_action_kind_for_decision(
    decision: GuardianDecisionKind,
) -> crate::guardian::GuardianActionKind {
    match decision {
        GuardianDecisionKind::Allow => crate::guardian::GuardianActionKind::Allow,
        GuardianDecisionKind::Warn => crate::guardian::GuardianActionKind::Warn,
        GuardianDecisionKind::Repair => crate::guardian::GuardianActionKind::Repair,
        GuardianDecisionKind::Retry => crate::guardian::GuardianActionKind::Retry,
        GuardianDecisionKind::Replace => crate::guardian::GuardianActionKind::Replace,
        GuardianDecisionKind::Strip => crate::guardian::GuardianActionKind::Strip,
        GuardianDecisionKind::Downgrade => crate::guardian::GuardianActionKind::Downgrade,
        GuardianDecisionKind::Degrade => crate::guardian::GuardianActionKind::Degrade,
        GuardianDecisionKind::Fallback => crate::guardian::GuardianActionKind::Fallback,
        GuardianDecisionKind::Quarantine => crate::guardian::GuardianActionKind::Quarantine,
        GuardianDecisionKind::Rollback => crate::guardian::GuardianActionKind::Rollback,
        GuardianDecisionKind::Block => crate::guardian::GuardianActionKind::Block,
        GuardianDecisionKind::AskUser => crate::guardian::GuardianActionKind::AskUser,
        GuardianDecisionKind::RecordOnly => crate::guardian::GuardianActionKind::RecordOnly,
    }
}

fn failure_memory_outcome_for_decision(
    decision: GuardianDecisionKind,
) -> FailureMemoryActionOutcome {
    match decision {
        GuardianDecisionKind::Retry => FailureMemoryActionOutcome::Retried,
        GuardianDecisionKind::Repair => FailureMemoryActionOutcome::Repaired,
        GuardianDecisionKind::Degrade => FailureMemoryActionOutcome::Degraded,
        GuardianDecisionKind::Fallback => FailureMemoryActionOutcome::Degraded,
        GuardianDecisionKind::Quarantine => FailureMemoryActionOutcome::Quarantined,
        GuardianDecisionKind::Rollback => FailureMemoryActionOutcome::RolledBack,
        GuardianDecisionKind::Block => FailureMemoryActionOutcome::Blocked,
        GuardianDecisionKind::RecordOnly => FailureMemoryActionOutcome::NotNeeded,
        _ => FailureMemoryActionOutcome::Failed,
    }
}

fn prefixed_token_fact(prefix: &str, value: &str, fallback: &str, max_len: usize) -> String {
    let value = sanitize_evidence_token(value, RedactionAudience::UserVisible, max_len)
        .unwrap_or_else(|| fallback.to_string());
    format!("{prefix}{value}")
}

fn prefixed_text_fact(prefix: &str, value: &str, fallback: &str, max_len: usize) -> String {
    let value = sanitize_evidence_text(value, RedactionAudience::UserVisible, max_len)
        .unwrap_or_else(|| fallback.to_string());
    format!("{prefix}{value}")
}

fn guardian_decision_kind_id(decision: GuardianDecisionKind) -> &'static str {
    match decision {
        GuardianDecisionKind::Allow => "allow",
        GuardianDecisionKind::Warn => "warn",
        GuardianDecisionKind::Repair => "repair",
        GuardianDecisionKind::Retry => "retry",
        GuardianDecisionKind::Replace => "replace",
        GuardianDecisionKind::Strip => "strip",
        GuardianDecisionKind::Downgrade => "downgrade",
        GuardianDecisionKind::Degrade => "degrade",
        GuardianDecisionKind::Fallback => "fallback",
        GuardianDecisionKind::Quarantine => "quarantine",
        GuardianDecisionKind::Rollback => "rollback",
        GuardianDecisionKind::Block => "block",
        GuardianDecisionKind::AskUser => "ask_user",
        GuardianDecisionKind::RecordOnly => "record_only",
    }
}

pub(super) fn public_install_id(id: &str) -> String {
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

pub(super) fn install_journal_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

pub(super) fn install_failure_point_from_journal(entry: &OperationJournalEntry) -> Option<String> {
    entry.failure_point.as_deref().and_then(|failure_point| {
        sanitize_evidence_token(failure_point, RedactionAudience::UserVisible, 96)
    })
}

pub(super) fn install_progress_history_from_journal(
    entry: &OperationJournalEntry,
) -> Vec<DownloadProgress> {
    let mut history = entry
        .completed_steps
        .iter()
        .filter_map(progress_from_install_journal_step)
        .collect::<Vec<_>>();

    if install_journal_is_terminal(entry.status) && !history.iter().any(|progress| progress.done) {
        history.push(terminal_progress_for_journal_status(entry.status));
    }

    history
}

fn progress_from_install_journal_step(step: &OperationJournalStep) -> Option<DownloadProgress> {
    let phase = install_phase_fact_value(step)?;
    let done = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_done:true");
    let failed = step
        .generated_facts
        .iter()
        .any(|fact| fact == "install_error:true")
        || step.result == OperationStepResult::Failed;

    Some(DownloadProgress {
        phase,
        current: if done && !failed { 1 } else { 0 },
        total: if done && !failed { 1 } else { 0 },
        file: None,
        error: (done && failed).then(|| INSTALL_FAILURE_MESSAGE.to_string()),
        done,
    })
}

fn install_phase_fact_value(step: &OperationJournalStep) -> Option<String> {
    step.generated_facts.iter().find_map(|fact| {
        fact.strip_prefix("install_phase:")
            .and_then(|phase| sanitize_evidence_token(phase, RedactionAudience::UserVisible, 48))
    })
}

fn terminal_progress_for_journal_status(status: OperationStatus) -> DownloadProgress {
    if status == OperationStatus::Succeeded {
        return DownloadProgress {
            phase: "done".to_string(),
            current: 1,
            total: 1,
            file: None,
            error: None,
            done: true,
        };
    }

    observed_install_failure_progress()
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

pub(super) fn latest_generated_fact_value(
    entry: &OperationJournalEntry,
    prefix: &str,
) -> Option<String> {
    entry
        .completed_steps
        .iter()
        .rev()
        .flat_map(|step| step.generated_facts.iter().rev())
        .find_map(|fact| fact.strip_prefix(prefix).map(str::to_string))
}
