use super::{
    INSTALL_FAILURE_MESSAGE, InstallJournalReconciliation, InstallProgressStepViewModel,
    InstallProgressViewModel, InstallVersionStaging, reconcile_install_journal_transition,
};
use crate::application::{
    ApplicationCommandRequest, CommandResult, CommandResultCarriers, InstallVersionCommand,
    InstallVersionPayload, OperationCommandCarrier,
};
use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianMode, GuardianPolicyContext, diagnose,
    guardian_install_outcome_fact_group, guardian_install_outcome_from_persisted_group,
    guardian_install_outcome_persistence_facts,
    install_artifact_failure_from_minecraft_download_fact, install_artifact_failure_guardian_fact,
    install_artifact_failure_guardian_outcome_with_context, install_artifact_failure_safety_case,
};
use crate::observability::{
    RedactionAudience, sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
};
use crate::state::{
    GuardianFailureMemoryStore, InstallProgressRecord, OperationJournalStore,
    OperationJournalStoreError, operation_journal_completed_step_is_visible,
    operation_journal_plan_is_visible,
};
use axial_minecraft::download::{ExecutionDownloadFact, ExecutionDownloadFactKind};
use axial_minecraft::{DownloadError, DownloadProgress};
use axial_minecraft::{LoaderError, LoaderInstallFailureKind};
use serde_json::{Value, json};
use tracing::warn;

const PROVIDER_FAILURE_SUPPRESSION_COOLDOWN_MINUTES: i64 = 5;
const PROVIDER_FAILURE_MEMORY_SOURCE: &str = "install_provider";
const COALESCED_PROGRESS_EVENT_INTERVAL: usize = 25;
const ROSETTA_INSTALL_COMMAND: &str = "softwareupdate --install-rosetta --agree-to-license";
const ROSETTA_REQUIRED_INSTALL_GUIDANCE: &str = "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.";
const RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs a Java runtime that is not available for this device.";
const RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX: &str =
    "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.";

async fn reconcile_install_journal_error(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<InstallJournalReconciliation, OperationJournalStoreError> {
    reconcile_install_journal_transition(journals, operation_id, error, expected).await
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

pub async fn begin_install_operation_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    version_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let expected = planned_install_journal(operation_id, version_id);
    match journals.create(expected.clone()).await {
        Ok(()) => Ok(()),
        Err(OperationJournalStoreError::AlreadyExists)
            if journals
                .get(operation_id)
                .is_some_and(|entry| operation_journal_plan_is_visible(&entry, &expected)) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(super) fn planned_install_journal(
    operation_id: &OperationId,
    version_id: &str,
) -> OperationJournalEntry {
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
    entry
}

pub async fn record_install_operation_progress(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
    last_recorded_phase: &mut Option<String>,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let terminal = progress.done;
    if !terminal && last_recorded_phase.as_deref() == Some(phase.as_str()) {
        return Ok(());
    }

    loop {
        let step_result = if terminal && progress.error.is_some() {
            OperationStepResult::Failed
        } else {
            OperationStepResult::Completed
        };
        let step = install_progress_step(&phase, step_result, progress);
        let failure_point = terminal
            .then(|| {
                progress
                    .error
                    .as_ref()
                    .map(|_| format!("install_progress_{phase}"))
            })
            .flatten();
        let result = if terminal && progress.error.is_some() {
            journals
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point
                        .as_deref()
                        .expect("failed progress has failure point"),
                    OperationOutcome::Failed,
                )
                .await
        } else if terminal {
            journals
                .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                .await
        } else {
            journals.record_progress(operation_id, step.clone()).await
        };

        match result {
            Ok(()) => {
                *last_recorded_phase = Some(phase);
                return Ok(());
            }
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_progress_transition_matches(
                        entry,
                        operation_id,
                        &step,
                        terminal,
                        failure_point.as_deref(),
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => {
                        *last_recorded_phase = Some(phase);
                        return Ok(());
                    }
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub async fn record_install_operation_interrupted(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress: &DownloadProgress,
) -> Result<(), OperationJournalStoreError> {
    let phase = safe_progress_phase(&progress.phase);
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        "install_worker_interrupted",
        GuardianInstallArtifactFailureKind::NetworkFailure,
    )
    .with_field("phase", phase);
    let (fact_ids, diagnosis_ids) = install_guardian_evidence_update(
        None,
        operation_id,
        &[evidence],
        OperationPhase::Downloading,
        &chrono::Utc::now().to_rfc3339(),
    )
    .unwrap_or_default();
    let step = install_progress_step(
        &safe_progress_phase(&progress.phase),
        OperationStepResult::Failed,
        progress,
    );
    loop {
        match journals
            .record_failure_with_guardian_evidence(
                operation_id,
                step.clone(),
                "install_worker_interrupted",
                OperationOutcome::Failed,
                fact_ids.clone(),
                diagnosis_ids.clone(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        &step,
                        "install_worker_interrupted",
                        &fact_ids,
                        &diagnosis_ids,
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub(super) async fn record_install_operation_initialization_cancelled(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    let progress = interrupted_install_progress();
    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        "install_initialization_cancelled",
        GuardianInstallArtifactFailureKind::NetworkFailure,
    )
    .with_field("phase", "initializing");
    let (fact_ids, diagnosis_ids) = install_guardian_evidence_update(
        None,
        operation_id,
        &[evidence],
        OperationPhase::Downloading,
        &chrono::Utc::now().to_rfc3339(),
    )
    .unwrap_or_default();
    let step = install_progress_step("initializing", OperationStepResult::Failed, &progress);
    loop {
        match journals
            .record_failure_with_guardian_evidence(
                operation_id,
                step.clone(),
                "install_initialization_cancelled",
                OperationOutcome::Failed,
                fact_ids.clone(),
                diagnosis_ids.clone(),
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_failure_with_evidence_matches(
                        entry,
                        operation_id,
                        &step,
                        "install_initialization_cancelled",
                        &fact_ids,
                        &diagnosis_ids,
                    )
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

pub async fn record_install_operation_guardian_evidence(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) -> Result<(), OperationJournalStoreError> {
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
        return Ok(());
    }

    let fact_ids = guardian_facts
        .iter()
        .map(|fact| format!("guardian_fact:{}", fact.id.as_str()))
        .collect::<Vec<_>>();
    let diagnosis_ids = diagnose(&guardian_facts, OperationPhase::Downloading)
        .into_iter()
        .map(|diagnosis| diagnosis.id())
        .collect::<Vec<_>>();
    record_guardian_evidence_with_reconciliation(journals, operation_id, fact_ids, diagnosis_ids)
        .await
}

pub async fn record_install_operation_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
) -> Result<(), OperationJournalStoreError> {
    let evidence = install_failure_evidence_from_download_facts(operation_id, facts);
    record_install_operation_guardian_failure_outcome_from_evidence(
        journals,
        operation_id,
        &evidence,
    )
    .await
}

pub async fn record_install_operation_guardian_failure_outcome_with_memory(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    operation_id: &OperationId,
    facts: &[ExecutionDownloadFact],
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    let evidence = install_failure_evidence_from_download_facts(operation_id, facts);
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        Some(failure_memory),
        operation_id,
        &evidence,
        OperationPhase::Downloading,
        observed_at,
    )
    .await
}

pub async fn record_install_operation_guardian_failure_outcome_for_error_with_memory(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    operation_id: &OperationId,
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    let evidence =
        install_failure_evidence_from_download_error_or_facts(operation_id, error, facts);
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        Some(failure_memory),
        operation_id,
        &evidence,
        OperationPhase::Downloading,
        observed_at,
    )
    .await
}

pub async fn record_loader_install_operation_guardian_failure_outcome(
    journals: &OperationJournalStore,
    failure_memory: &GuardianFailureMemoryStore,
    operation_id: &OperationId,
    target_id: &str,
    error: &LoaderError,
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    match loader_install_failure_disposition(error.failure_kind()) {
        LoaderGuardianFailureDisposition::Evidence {
            kind,
            ownership,
            phase,
        } => {
            let evidence = loader_error_guardian_failure_evidence(
                operation_id,
                target_id,
                error,
                kind,
                ownership,
            );
            record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
                journals,
                Some(failure_memory),
                operation_id,
                &[evidence],
                phase,
                observed_at,
            )
            .await
        }
        LoaderGuardianFailureDisposition::DelegatedBaseInstall
        | LoaderGuardianFailureDisposition::DelegatedArtifactDownload
        | LoaderGuardianFailureDisposition::PreOperation => Ok(()),
    }
}

pub async fn record_loader_base_install_dependency_guardian_failure_outcome(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    target_id: &str,
    base_version_id: &str,
) -> Result<(), OperationJournalStoreError> {
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
    )
    .await
}

pub fn install_guardian_outcome_summary_from_journal(
    entry: &OperationJournalEntry,
) -> Option<crate::guardian::GuardianInstallOutcomeSummary> {
    let fact_group = entry.completed_steps.iter().rev().find_map(|step| {
        guardian_install_outcome_fact_group(step.generated_facts.iter().map(String::as_str))
    })?;
    let diagnosis_id = entry
        .guardian_diagnosis_ids
        .iter()
        .copied()
        .rev()
        .find(|id| *id != DiagnosisId::LauncherManagedArtifactCorrupt)?;
    guardian_install_outcome_from_persisted_group(diagnosis_id, fact_group)
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
            if is_specific_terminal_install_failure_message(&error) {
                return error;
            }
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

pub(crate) fn install_progress_with_terminal_error(
    mut progress: DownloadProgress,
    error: &DownloadError,
) -> DownloadProgress {
    if progress.done
        && progress.error.is_some()
        && let Some(message) = specific_terminal_install_failure_message(error)
    {
        progress.error = Some(message);
    }
    progress
}

fn specific_terminal_install_failure_message(error: &DownloadError) -> Option<String> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(runtime_unavailable_install_failure_message(
            component, platform,
        )),
        DownloadError::RuntimeRosettaRequired { component } => {
            Some(runtime_rosetta_required_install_failure_message(component))
        }
        _ => None,
    }
}

fn runtime_unavailable_install_failure_message(component: &str, platform: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    let platform = sanitize_evidence_token(platform, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "this-device".to_string());
    format!(
        "{RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component} on {platform}."
    )
}

fn runtime_rosetta_required_install_failure_message(component: &str) -> String {
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "required-runtime".to_string());
    format!(
        "{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: {component}. Install Rosetta 2 by running `{ROSETTA_INSTALL_COMMAND}` in Terminal, then retry."
    )
}

fn is_specific_terminal_install_failure_message(error: &str) -> bool {
    if error.starts_with(RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX) {
        return is_rosetta_required_terminal_install_failure_message(error);
    }

    error.starts_with(RUNTIME_UNAVAILABLE_INSTALL_FAILURE_MESSAGE_PREFIX)
        && sanitize_public_diagnostic_text(
            error,
            RedactionAudience::UserVisible,
            220,
            INSTALL_FAILURE_MESSAGE,
        )
        .as_str()
            == error
}

fn is_rosetta_required_terminal_install_failure_message(error: &str) -> bool {
    let prefix =
        format!("{RUNTIME_ROSETTA_REQUIRED_INSTALL_FAILURE_MESSAGE_PREFIX} Required runtime: ");
    let suffix = format!(". {ROSETTA_REQUIRED_INSTALL_GUIDANCE}");
    let Some(component) = error
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(&suffix))
    else {
        return false;
    };

    sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)
        .is_some_and(|sanitized| sanitized == component)
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

pub(crate) fn install_progress_record(progress: DownloadProgress) -> InstallProgressRecord {
    let vanilla_event_json =
        serde_json::to_string(&public_vanilla_install_progress_json(&progress))
            .unwrap_or_else(|_| "{}".to_string());
    let loader_event_json = serde_json::to_string(&public_loader_install_progress_json(&progress))
        .unwrap_or_else(|_| "{}".to_string());
    InstallProgressRecord::with_event_json(progress, vanilla_event_json, loader_event_json)
}

#[derive(Default)]
pub(crate) struct InstallProgressCoalescer {
    last_emitted: Option<DownloadProgress>,
    pending: Option<DownloadProgress>,
    pending_count: usize,
}

impl InstallProgressCoalescer {
    pub(crate) fn push(&mut self, progress: DownloadProgress) -> Vec<DownloadProgress> {
        if should_passthrough_progress(&progress) || self.is_phase_transition(&progress) {
            let mut emitted = self.flush_vec();
            emitted.push(self.mark_emitted(progress));
            return emitted;
        }

        if self.should_emit_coalesced_now(&progress) {
            self.pending = None;
            self.pending_count = 0;
            return vec![self.mark_emitted(progress)];
        }

        self.pending = Some(progress);
        self.pending_count = self.pending_count.saturating_add(1);
        if self.pending_count >= COALESCED_PROGRESS_EVENT_INTERVAL {
            return self.flush_vec();
        }

        Vec::new()
    }

    pub(crate) fn flush(&mut self) -> Option<DownloadProgress> {
        let progress = self.pending.take()?;
        self.pending_count = 0;
        Some(self.mark_emitted(progress))
    }

    fn flush_vec(&mut self) -> Vec<DownloadProgress> {
        self.flush().into_iter().collect()
    }

    fn mark_emitted(&mut self, progress: DownloadProgress) -> DownloadProgress {
        self.last_emitted = Some(progress.clone());
        progress
    }

    fn is_phase_transition(&self, progress: &DownloadProgress) -> bool {
        self.pending
            .as_ref()
            .or(self.last_emitted.as_ref())
            .is_some_and(|previous| previous.phase != progress.phase)
    }

    fn should_emit_coalesced_now(&self, progress: &DownloadProgress) -> bool {
        let Some(last) = self.last_emitted.as_ref() else {
            return true;
        };
        progress.bytes_total != last.bytes_total
            || progress.total != last.total
            || byte_progress_bucket(progress) != byte_progress_bucket(last)
            || (progress.total > 0 && progress.current >= progress.total)
    }
}

fn should_passthrough_progress(progress: &DownloadProgress) -> bool {
    progress.done
        || progress.error.is_some()
        || matches!(
            progress.phase.as_str(),
            "done" | "error" | "java_runtime_ready"
        )
        || !is_coalesced_progress_phase(progress.phase.as_str())
}

fn is_coalesced_progress_phase(phase: &str) -> bool {
    matches!(phase, "libraries" | "loader_libraries" | "java_runtime")
}

fn byte_progress_bucket(progress: &DownloadProgress) -> Option<u8> {
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    Some((((u128::from(done.min(total)) * 100) / u128::from(total)).min(100)) as u8)
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
        "java_runtime" => java_runtime_label(progress),
        "java_runtime_ready" => "Java runtime ready".to_string(),
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
    if let Some(pct) = byte_weighted_install_pct(progress, kind) {
        return pct;
    }
    // Fallback for events without transfer-plan facts: pre-plan phases
    // (version_json), loader-specific head phases (loader metadata, installer
    // artifacts, processors), and journal-replayed history.
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
        _ => 0,
    };
    pct.clamp(0, 100) as u8
}

/// Overall progress from the installer's transfer-plan facts: bytes of
/// planned work completed across every concurrent phase (client jar,
/// libraries, assets, managed Java runtime). Capped below 100 so only the
/// terminal `done` event completes the bar. Loader installs reserve a head
/// span for the loader-specific phases, which carry no byte facts.
fn byte_weighted_install_pct(progress: &DownloadProgress, kind: InstallProgressKind) -> Option<u8> {
    if matches!(progress.phase.as_str(), "done" | "error") {
        return None;
    }
    let (done, total) = (progress.bytes_done?, progress.bytes_total?);
    if total == 0 {
        return None;
    }
    let fraction = (done.min(total) as f64) / (total as f64);
    let (base, span) = match kind {
        InstallProgressKind::Vanilla => (0.0, 99.0),
        InstallProgressKind::Loader => (20.0, 79.0),
    };
    Some((base + fraction * span).round().clamp(0.0, 99.0) as u8)
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

fn java_runtime_label(progress: &DownloadProgress) -> String {
    if progress.total > 0 {
        count_label("Java runtime files", progress)
    } else {
        "Preparing Java runtime".to_string()
    }
}

fn progress_fraction(progress: &DownloadProgress) -> f32 {
    if progress.total <= 0 {
        return 0.0;
    }
    (progress.current.max(0) as f32 / progress.total as f32).clamp(0.0, 1.0)
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

fn install_failure_evidence_from_download_error_or_facts(
    operation_id: &OperationId,
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<GuardianInstallArtifactFailureEvidence> {
    if let Some(evidence) = typed_runtime_failure_evidence(operation_id, error) {
        return vec![evidence];
    }

    let terminal_facts = terminal_download_failure_facts_for_error(error, facts);
    let terminal_fact_evidence =
        install_failure_evidence_from_download_facts(operation_id, &terminal_facts);
    if should_prefer_terminal_download_facts(error) && !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    if let Some(evidence) = install_failure_evidence_from_download_error(operation_id, error) {
        return vec![evidence];
    }

    if !terminal_fact_evidence.is_empty() {
        return terminal_fact_evidence;
    }

    install_failure_evidence_from_download_facts(operation_id, facts)
}

fn typed_runtime_failure_evidence(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    match error {
        DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        } => Some(
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(operation_id.clone()),
                format!("java_runtime_{component}_{platform}"),
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", component.as_str())
            .with_field("platform", platform.as_str()),
        ),
        DownloadError::RuntimeRosettaRequired { component } => Some(
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(operation_id.clone()),
                format!("java_runtime_{component}_rosetta"),
                GuardianInstallArtifactFailureKind::RuntimeRosettaRequired,
            )
            .with_field("component", component.as_str()),
        ),
        _ => None,
    }
}

fn install_failure_evidence_from_download_error(
    operation_id: &OperationId,
    error: &DownloadError,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    let (target_id, kind) = install_failure_target_and_kind_from_download_error(error)?;

    Some(GuardianInstallArtifactFailureEvidence::launcher_managed(
        Some(operation_id.clone()),
        target_id,
        kind,
    ))
}

fn install_failure_target_and_kind_from_download_error(
    error: &DownloadError,
) -> Option<(&'static str, GuardianInstallArtifactFailureKind)> {
    let evidence = match error {
        DownloadError::FileOperation(_) => (
            "install_filesystem",
            GuardianInstallArtifactFailureKind::PermissionDenied,
        ),
        DownloadError::ResolveManifest(_) => (
            "version_manifest",
            GuardianInstallArtifactFailureKind::ProviderFailure,
        ),
        DownloadError::Request(_) => (
            "minecraft_download",
            GuardianInstallArtifactFailureKind::NetworkFailure,
        ),
        DownloadError::ParseVersion(_) => (
            "version_json",
            GuardianInstallArtifactFailureKind::MetadataInvalid,
        ),
        DownloadError::PrepareRuntime(_) => (
            "java_runtime",
            GuardianInstallArtifactFailureKind::ProviderFailure,
        ),
        DownloadError::RuntimeRosettaRequired { .. }
        | DownloadError::RuntimeUnavailableForPlatform { .. } => return None,
        DownloadError::Integrity(_) => return None,
    };

    Some(evidence)
}

pub(super) fn install_repair_facts_from_download_error_or_facts(
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionDownloadFact> {
    if matches!(
        error,
        DownloadError::RuntimeRosettaRequired { .. }
            | DownloadError::RuntimeUnavailableForPlatform { .. }
    ) {
        return Vec::new();
    }

    let terminal_facts = terminal_download_failure_facts_for_error(error, facts);
    if should_prefer_terminal_download_facts(error) && !terminal_facts.is_empty() {
        return terminal_facts;
    }

    if install_failure_target_and_kind_from_download_error(error).is_some() {
        return Vec::new();
    }

    if !terminal_facts.is_empty() {
        return terminal_facts;
    }

    Vec::new()
}

fn terminal_download_failure_facts_for_error(
    error: &DownloadError,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionDownloadFact> {
    facts
        .iter()
        .filter(|fact| terminal_download_failure_fact_kind_for_error(error, fact.kind))
        .cloned()
        .collect()
}

fn should_prefer_terminal_download_facts(error: &DownloadError) -> bool {
    matches!(
        error,
        DownloadError::FileOperation(_)
            | DownloadError::Request(_)
            | DownloadError::PrepareRuntime(_)
            | DownloadError::Integrity(_)
    )
}

fn terminal_download_failure_fact_kind_for_error(
    error: &DownloadError,
    kind: ExecutionDownloadFactKind,
) -> bool {
    if matches!(error, DownloadError::Request(_)) {
        return request_terminal_download_failure_fact_kind(kind);
    }

    terminal_download_failure_fact_kind(kind)
}

fn request_terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::ProviderFailure
    )
}

fn terminal_download_failure_fact_kind(kind: ExecutionDownloadFactKind) -> bool {
    matches!(
        kind,
        ExecutionDownloadFactKind::ChecksumMismatch
            | ExecutionDownloadFactKind::MetadataInvalid
            | ExecutionDownloadFactKind::MetadataMissing
            | ExecutionDownloadFactKind::Interrupted
            | ExecutionDownloadFactKind::NetworkFailure
            | ExecutionDownloadFactKind::OwnershipRefused
            | ExecutionDownloadFactKind::PermissionFailure
            | ExecutionDownloadFactKind::PromoteFailed
            | ExecutionDownloadFactKind::ProviderFailure
            | ExecutionDownloadFactKind::SizeMismatch
            | ExecutionDownloadFactKind::TempWriteFailed
    )
}

async fn record_install_operation_guardian_failure_outcome_from_evidence(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
) -> Result<(), OperationJournalStoreError> {
    record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
        journals,
        None,
        operation_id,
        evidence,
        OperationPhase::Downloading,
        &chrono::Utc::now().to_rfc3339(),
    )
    .await
}

async fn record_install_operation_guardian_failure_outcome_from_evidence_with_memory(
    journals: &OperationJournalStore,
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    observed_at: &str,
) -> Result<(), OperationJournalStoreError> {
    let Some((facts, diagnosis_ids)) = install_guardian_evidence_update(
        failure_memory,
        operation_id,
        evidence,
        phase,
        observed_at,
    ) else {
        return Ok(());
    };
    record_guardian_evidence_with_reconciliation(journals, operation_id, facts, diagnosis_ids).await
}

async fn record_guardian_evidence_with_reconciliation(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    facts: Vec<String>,
    diagnosis_ids: Vec<DiagnosisId>,
) -> Result<(), OperationJournalStoreError> {
    loop {
        match journals
            .record_guardian_evidence(operation_id, facts.clone(), diagnosis_ids.clone())
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                match reconcile_install_journal_error(journals, operation_id, error, |entry| {
                    install_journal_identity_matches(entry, operation_id)
                        && facts
                            .iter()
                            .all(|fact| install_entry_contains_fact(entry, fact))
                        && diagnosis_ids
                            .iter()
                            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
                })
                .await?
                {
                    InstallJournalReconciliation::MutationCommitted => return Ok(()),
                    InstallJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

fn install_journal_identity_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
) -> bool {
    entry.operation_id == *operation_id
        && entry.command == CommandKind::InstallVersion
        && entry.owner == StabilizationSystem::Application
        && entry.ownership == OwnershipClass::LauncherManaged
}

fn install_progress_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    step: &OperationJournalStep,
    terminal: bool,
    failure_point: Option<&str>,
) -> bool {
    if !install_journal_identity_matches(entry, operation_id)
        || !operation_journal_completed_step_is_visible(entry, step)
    {
        return false;
    }
    if !terminal {
        return entry.status == OperationStatus::Running
            && entry.outcome.is_none()
            && entry.failure_point.is_none();
    }
    if let Some(failure_point) = failure_point {
        entry.status == OperationStatus::Failed
            && entry.outcome == Some(OperationOutcome::Failed)
            && entry.failure_point.as_deref() == Some(failure_point)
    } else {
        entry.status == OperationStatus::Succeeded
            && entry.outcome == Some(OperationOutcome::Succeeded)
            && entry.failure_point.is_none()
    }
}

fn install_failure_with_evidence_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    step: &OperationJournalStep,
    failure_point: &str,
    fact_ids: &[String],
    diagnosis_ids: &[DiagnosisId],
) -> bool {
    let mut expected_step = step.clone();
    for fact_id in fact_ids {
        if !expected_step.generated_facts.contains(fact_id) {
            expected_step.generated_facts.push(fact_id.clone());
        }
    }
    install_journal_identity_matches(entry, operation_id)
        && entry.status == OperationStatus::Failed
        && entry.outcome == Some(OperationOutcome::Failed)
        && entry.failure_point.as_deref() == Some(failure_point)
        && operation_journal_completed_step_is_visible(entry, &expected_step)
        && diagnosis_ids
            .iter()
            .all(|diagnosis_id| entry.guardian_diagnosis_ids.contains(diagnosis_id))
}

fn install_entry_contains_fact(entry: &OperationJournalEntry, fact: &str) -> bool {
    entry
        .completed_steps
        .last()
        .is_some_and(|step| step.generated_facts.iter().any(|existing| existing == fact))
}

fn install_guardian_evidence_update(
    failure_memory: Option<&GuardianFailureMemoryStore>,
    operation_id: &OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    phase: OperationPhase,
    observed_at: &str,
) -> Option<(Vec<String>, Vec<DiagnosisId>)> {
    let mode = GuardianMode::Managed;
    let context = failure_memory_suppression_context(
        failure_memory,
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        observed_at,
    );
    let outcome = install_artifact_failure_guardian_outcome_with_context(
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        context,
    )?;

    record_provider_failure_memory_if_needed(
        failure_memory,
        Some(operation_id.clone()),
        mode,
        phase,
        evidence,
        &outcome,
        observed_at,
    );

    let facts = guardian_install_outcome_persistence_facts(&outcome.user_outcome);

    Some((facts, vec![outcome.diagnosis_id]))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoaderGuardianFailureDisposition {
    Evidence {
        kind: GuardianInstallArtifactFailureKind,
        ownership: OwnershipClass,
        phase: OperationPhase,
    },
    DelegatedBaseInstall,
    DelegatedArtifactDownload,
    PreOperation,
}

pub(crate) const fn loader_install_failure_disposition(
    failure_kind: LoaderInstallFailureKind,
) -> LoaderGuardianFailureDisposition {
    use LoaderGuardianFailureDisposition::{
        DelegatedArtifactDownload, DelegatedBaseInstall, Evidence, PreOperation,
    };

    match failure_kind {
        LoaderInstallFailureKind::InvalidMinecraftVersion
        | LoaderInstallFailureKind::InvalidBuildId
        | LoaderInstallFailureKind::CatalogUnavailable
        | LoaderInstallFailureKind::CatalogStale
        | LoaderInstallFailureKind::BuildNotFound => PreOperation,
        LoaderInstallFailureKind::BaseInstallFailed => DelegatedBaseInstall,
        LoaderInstallFailureKind::ArtifactDownloadFailed => DelegatedArtifactDownload,
        LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderRateLimited
        | LoaderInstallFailureKind::ArtifactMissing => Evidence {
            kind: GuardianInstallArtifactFailureKind::ProviderFailure,
            ownership: OwnershipClass::ExternalProviderDerived,
            phase: OperationPhase::Downloading,
        },
        LoaderInstallFailureKind::ProviderNetworkFailure => Evidence {
            kind: GuardianInstallArtifactFailureKind::NetworkFailure,
            ownership: OwnershipClass::ExternalProviderDerived,
            phase: OperationPhase::Downloading,
        },
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid
        | LoaderInstallFailureKind::InvalidProfile => Evidence {
            kind: GuardianInstallArtifactFailureKind::MetadataInvalid,
            ownership: OwnershipClass::ExternalProviderDerived,
            phase: OperationPhase::Downloading,
        },
        LoaderInstallFailureKind::ParseFailed
        | LoaderInstallFailureKind::VerifyFailed
        | LoaderInstallFailureKind::InstallExecutionFailed => Evidence {
            kind: GuardianInstallArtifactFailureKind::ExecutionFailed,
            ownership: OwnershipClass::LauncherManaged,
            phase: OperationPhase::Installing,
        },
        LoaderInstallFailureKind::ProcessorFailed => Evidence {
            kind: GuardianInstallArtifactFailureKind::ProcessorFailed,
            ownership: OwnershipClass::LauncherManaged,
            phase: OperationPhase::Installing,
        },
    }
}

fn loader_error_guardian_failure_evidence(
    operation_id: &OperationId,
    target_id: &str,
    error: &LoaderError,
    kind: GuardianInstallArtifactFailureKind,
    ownership: OwnershipClass,
) -> GuardianInstallArtifactFailureEvidence {
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
    evidence
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
        .find(|diagnosis| diagnosis.id() == DiagnosisId::DownloadUnavailable)?;
    let target = diagnosis.affected_targets().first()?;
    let key = FailureMemoryKey::for_observation(
        diagnosis.domain(),
        &diagnosis.id(),
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
    if outcome.diagnosis_id != DiagnosisId::DownloadUnavailable {
        return;
    }
    let Some(memory) = failure_memory else {
        return;
    };
    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let Some(diagnosis) = safety_case
        .diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == outcome.diagnosis_id)
    else {
        return;
    };
    let Some(target) = diagnosis.affected_targets().first().cloned() else {
        return;
    };
    let suppression_until = provider_failure_suppression_until(observed_at);
    let action_outcome = match outcome.decision {
        GuardianActionKind::Retry => FailureMemoryActionOutcome::Retried,
        GuardianActionKind::Block => FailureMemoryActionOutcome::Blocked,
        GuardianActionKind::AskUser => FailureMemoryActionOutcome::Failed,
        _ => return,
    };
    let entry = GuardianFailureMemoryEntry::observed(
        diagnosis.id(),
        diagnosis.domain(),
        target,
        mode,
        Some(PROVIDER_FAILURE_MEMORY_SOURCE),
        observed_at.to_string(),
    )
    .with_action(outcome.decision, action_outcome)
    .with_suppression_until(suppression_until);
    if let Err(error) = memory.record(entry) {
        warn!(
            error_kind = error.class(),
            "failed to record Guardian install-provider failure memory"
        );
    }
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
        bytes_done: None,
        bytes_total: None,
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
        bytes_done: None,
        bytes_total: None,
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
            bytes_done: None,
            bytes_total: None,
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
        | "java_runtime" | "java_runtime_ready" | "loader_meta" | "loader_json" | "artifacts"
        | "loader_libraries" => OperationPhase::Downloading,
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
