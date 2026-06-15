//! Application-owned install command staging and journal lifecycle helpers.
//!
//! This module wraps existing install workflows with command identity and
//! journal records. It does not alter provider resolution, downloading,
//! verification, retry, or repair behavior.

use super::{
    ApplicationCommand, ApplicationCommandRequest, CommandResult, CommandResultCarriers,
    InstallVersionCommand, InstallVersionPayload, OperationCommandCarrier,
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
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::{GuardianFailureMemoryStore, OperationJournalStore};
use croopor_minecraft::DownloadProgress;
use croopor_minecraft::download::{
    ExecutionDownloadFact, ExecutionDownloadFactKind, SelectedDownloadArtifactDescriptor,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

const LAUNCHER_MANAGED_ARTIFACT_CORRUPT_DIAGNOSIS: &str = "launcher_managed_artifact_corrupt";
const REPAIR_OPERATION_FACT_PREFIX: &str = "guardian_repair_operation:";
const REPAIR_STATUS_FACT_PREFIX: &str = "guardian_repair_status:";
const REPAIR_SUMMARY_FACT_PREFIX: &str = "guardian_repair_summary:";

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
