//! Guardian artifact repair execution.
//!
//! This executor consumes an already-built Guardian repair plan plus explicit
//! provider metadata. It does not discover providers or decide repair policy.

use super::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode, GuardianRepairPlan,
    GuardianRepairTaskKind,
};
use crate::execution::download::{
    DownloadChecksum, DownloadChecksumAlgorithm, DownloadToTempRequest, download_url_to_temp,
    valid_download_checksum_metadata,
};
use crate::execution::file::{QuarantineFileRequest, quarantine_launcher_managed_file};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{RedactionAudience, sanitize_evidence_token};
use crate::state::OperationJournalStore;
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, RollbackState,
    StabilizationSystem, TargetDescriptor,
};
use crate::state::failure_memory::{
    FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
    GuardianFailureMemoryStore,
};
use chrono::{DateTime, Duration, FixedOffset};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;

const ARTIFACT_REPAIR_MAX_ATTEMPTS: u32 = 1;
const DEFAULT_ARTIFACT_REPAIR_SUPPRESSION_MINUTES: i64 = 15;

pub struct GuardianArtifactRepairRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub plan: &'a GuardianRepairPlan,
    pub destination: &'a Path,
    pub source: GuardianArtifactRepairSource<'a>,
    pub client: &'a Client,
    pub journals: &'a OperationJournalStore,
    pub failure_memory: &'a GuardianFailureMemoryStore,
    pub mode: GuardianMode,
    pub observed_at: &'a str,
}

#[derive(Clone, Debug)]
pub struct GuardianArtifactRepairSource<'a> {
    pub url: &'a str,
    pub checksum_algorithm: &'a str,
    pub expected_checksum: &'a str,
    pub expected_size: Option<u64>,
    pub max_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianArtifactRepairOutcome {
    pub operation_id: OperationId,
    pub diagnosis_id: DiagnosisId,
    pub action: GuardianActionKind,
    pub status: GuardianArtifactRepairStatus,
    pub facts: Vec<String>,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianArtifactRepairStatus {
    Repaired,
    Blocked,
    Failed,
    Suppressed,
}

pub async fn execute_guardian_artifact_repair(
    request: GuardianArtifactRepairRequest<'_>,
) -> GuardianArtifactRepairOutcome {
    let operation_id = request
        .operation_id
        .clone()
        .unwrap_or_else(new_repair_operation_id);
    let diagnosis_id = request.plan.diagnosis_id.clone();
    let target = request.plan.target.clone();

    if let Some(block_reason) = pre_execution_block_reason(&request) {
        create_terminal_journal(
            request.journals,
            &operation_id,
            request.plan,
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        record_artifact_repair_memory(
            request.failure_memory,
            &diagnosis_id,
            request.mode,
            &target,
            FailureMemoryActionOutcome::Blocked,
            request.observed_at,
            None,
            false,
            None,
        );
        return artifact_repair_outcome(
            operation_id,
            diagnosis_id,
            GuardianArtifactRepairStatus::Blocked,
            Vec::new(),
            block_reason,
        );
    }

    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Install,
        &diagnosis_id,
        &target,
        request.mode,
        None,
    );
    if let Some(entry) = request.failure_memory.get(&memory_key)
        && (suppression_active(&entry, request.observed_at)
            || entry.repair_attempt_count >= ARTIFACT_REPAIR_MAX_ATTEMPTS)
    {
        let suppression_until = entry
            .suppression_until
            .as_deref()
            .map(str::to_string)
            .or_else(|| default_suppression_until(request.observed_at));
        create_terminal_journal(
            request.journals,
            &operation_id,
            request.plan,
            OperationStatus::Blocked,
            OperationOutcome::Suppressed,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        record_artifact_repair_memory(
            request.failure_memory,
            &diagnosis_id,
            request.mode,
            &target,
            FailureMemoryActionOutcome::Suppressed,
            request.observed_at,
            suppression_until.as_deref(),
            false,
            None,
        );
        return artifact_repair_outcome(
            operation_id,
            diagnosis_id,
            GuardianArtifactRepairStatus::Suppressed,
            Vec::new(),
            "guardian_artifact_repair_suppressed",
        );
    }

    create_planned_journal(request.journals, &operation_id, request.plan);

    let quarantine_report = match quarantine_launcher_managed_file(QuarantineFileRequest {
        operation_id: Some(operation_id.clone()),
        target: target.clone(),
        source: request.destination,
    }) {
        Ok(report) => report,
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            request.journals.record_failure(
                &operation_id,
                repair_step(
                    "quarantine_launcher_managed_target",
                    OperationStepResult::Failed,
                    Some(target.clone()),
                    fact_ids.clone(),
                    RollbackState::Unavailable,
                ),
                "quarantine_launcher_managed_target",
                OperationOutcome::Failed,
            );
            record_artifact_repair_memory(
                request.failure_memory,
                &diagnosis_id,
                request.mode,
                &target,
                FailureMemoryActionOutcome::Failed,
                request.observed_at,
                default_suppression_until(request.observed_at).as_deref(),
                true,
                None,
            );
            return artifact_repair_outcome(
                operation_id,
                diagnosis_id,
                GuardianArtifactRepairStatus::Failed,
                fact_ids,
                "guardian_artifact_quarantine_failed",
            );
        }
    };
    let quarantine_facts = fact_ids(&quarantine_report.facts);
    request.journals.record_progress(
        &operation_id,
        repair_step(
            "quarantine_launcher_managed_target",
            OperationStepResult::Completed,
            Some(target.clone()),
            quarantine_facts,
            RollbackState::Available,
        ),
    );

    let checksum =
        source_download_checksum(&request.source).expect("source checksum validated before repair");
    let mut download_request =
        DownloadToTempRequest::new(target.clone(), request.destination, request.source.url)
            .with_expected_checksum(checksum);
    if let Some(max_bytes) = request.source.max_bytes {
        download_request = download_request.with_max_bytes(max_bytes);
    }
    if let Some(expected_size) = request.source.expected_size {
        download_request = download_request.with_expected_size(expected_size);
    }
    download_request.operation_id = Some(operation_id.clone());

    match download_url_to_temp(download_request, request.client).await {
        Ok(report) => {
            let fact_ids = fact_ids(&report.facts);
            request.journals.record_success(
                &operation_id,
                repair_step(
                    "promote_verified_artifact",
                    OperationStepResult::Completed,
                    Some(target.clone()),
                    fact_ids.clone(),
                    RollbackState::Available,
                ),
                OperationOutcome::Succeeded,
            );
            let quarantined_target = TargetDescriptor::new(
                StabilizationSystem::Execution,
                target.kind,
                format!("quarantine-{}", target.id),
                target.ownership,
            );
            record_artifact_repair_memory(
                request.failure_memory,
                &diagnosis_id,
                request.mode,
                &target,
                FailureMemoryActionOutcome::Repaired,
                request.observed_at,
                default_suppression_until(request.observed_at).as_deref(),
                true,
                Some(quarantined_target),
            );
            artifact_repair_outcome(
                operation_id,
                diagnosis_id,
                GuardianArtifactRepairStatus::Repaired,
                fact_ids,
                "guardian_artifact_repaired",
            )
        }
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            request.journals.record_failure(
                &operation_id,
                repair_step(
                    "download_artifact_to_temp",
                    OperationStepResult::Failed,
                    Some(target.clone()),
                    fact_ids.clone(),
                    RollbackState::Available,
                ),
                "download_artifact_to_temp",
                OperationOutcome::Failed,
            );
            record_artifact_repair_memory(
                request.failure_memory,
                &diagnosis_id,
                request.mode,
                &target,
                FailureMemoryActionOutcome::Failed,
                request.observed_at,
                default_suppression_until(request.observed_at).as_deref(),
                true,
                None,
            );
            artifact_repair_outcome(
                operation_id,
                diagnosis_id,
                GuardianArtifactRepairStatus::Failed,
                fact_ids,
                "guardian_artifact_redownload_failed",
            )
        }
    }
}

pub async fn execute_guardian_missing_artifact_repair(
    request: GuardianArtifactRepairRequest<'_>,
) -> GuardianArtifactRepairOutcome {
    let operation_id = request
        .operation_id
        .clone()
        .unwrap_or_else(new_repair_operation_id);
    let diagnosis_id = request.plan.diagnosis_id.clone();
    let target = request.plan.target.clone();

    if let Some(block_reason) = pre_missing_artifact_execution_block_reason(&request) {
        create_terminal_journal(
            request.journals,
            &operation_id,
            request.plan,
            OperationStatus::Blocked,
            OperationOutcome::Blocked,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        record_artifact_repair_memory(
            request.failure_memory,
            &diagnosis_id,
            request.mode,
            &target,
            FailureMemoryActionOutcome::Blocked,
            request.observed_at,
            None,
            false,
            None,
        );
        return artifact_repair_outcome(
            operation_id,
            diagnosis_id,
            GuardianArtifactRepairStatus::Blocked,
            Vec::new(),
            block_reason,
        );
    }

    let memory_key = FailureMemoryKey::for_observation(
        GuardianDomain::Install,
        &diagnosis_id,
        &target,
        request.mode,
        None,
    );
    if let Some(entry) = request.failure_memory.get(&memory_key)
        && (suppression_active(&entry, request.observed_at)
            || entry.repair_attempt_count >= ARTIFACT_REPAIR_MAX_ATTEMPTS)
    {
        let suppression_until = entry
            .suppression_until
            .as_deref()
            .map(str::to_string)
            .or_else(|| default_suppression_until(request.observed_at));
        create_terminal_journal(
            request.journals,
            &operation_id,
            request.plan,
            OperationStatus::Blocked,
            OperationOutcome::Suppressed,
            OperationStepResult::Skipped,
            Vec::new(),
        );
        record_artifact_repair_memory(
            request.failure_memory,
            &diagnosis_id,
            request.mode,
            &target,
            FailureMemoryActionOutcome::Suppressed,
            request.observed_at,
            suppression_until.as_deref(),
            false,
            None,
        );
        return artifact_repair_outcome(
            operation_id,
            diagnosis_id,
            GuardianArtifactRepairStatus::Suppressed,
            Vec::new(),
            "guardian_artifact_repair_suppressed",
        );
    }

    create_planned_journal(request.journals, &operation_id, request.plan);

    let checksum =
        source_download_checksum(&request.source).expect("source checksum validated before repair");
    let mut download_request =
        DownloadToTempRequest::new(target.clone(), request.destination, request.source.url)
            .with_expected_checksum(checksum);
    if let Some(max_bytes) = request.source.max_bytes {
        download_request = download_request.with_max_bytes(max_bytes);
    }
    if let Some(expected_size) = request.source.expected_size {
        download_request = download_request.with_expected_size(expected_size);
    }
    download_request.operation_id = Some(operation_id.clone());

    match download_url_to_temp(download_request, request.client).await {
        Ok(report) => {
            let fact_ids = fact_ids(&report.facts);
            request.journals.record_success(
                &operation_id,
                repair_step(
                    "promote_verified_artifact",
                    OperationStepResult::Completed,
                    Some(target.clone()),
                    fact_ids.clone(),
                    RollbackState::Available,
                ),
                OperationOutcome::Succeeded,
            );
            record_artifact_repair_memory(
                request.failure_memory,
                &diagnosis_id,
                request.mode,
                &target,
                FailureMemoryActionOutcome::Repaired,
                request.observed_at,
                default_suppression_until(request.observed_at).as_deref(),
                true,
                None,
            );
            artifact_repair_outcome(
                operation_id,
                diagnosis_id,
                GuardianArtifactRepairStatus::Repaired,
                fact_ids,
                "guardian_artifact_repaired",
            )
        }
        Err(error) => {
            let fact_ids = fact_ids(&error.facts);
            request.journals.record_failure(
                &operation_id,
                repair_step(
                    "download_artifact_to_temp",
                    OperationStepResult::Failed,
                    Some(target.clone()),
                    fact_ids.clone(),
                    RollbackState::Unavailable,
                ),
                "download_artifact_to_temp",
                OperationOutcome::Failed,
            );
            record_artifact_repair_memory(
                request.failure_memory,
                &diagnosis_id,
                request.mode,
                &target,
                FailureMemoryActionOutcome::Failed,
                request.observed_at,
                default_suppression_until(request.observed_at).as_deref(),
                true,
                None,
            );
            artifact_repair_outcome(
                operation_id,
                diagnosis_id,
                GuardianArtifactRepairStatus::Failed,
                fact_ids,
                "guardian_artifact_redownload_failed",
            )
        }
    }
}

fn pre_execution_block_reason(request: &GuardianArtifactRepairRequest<'_>) -> Option<&'static str> {
    if request.plan.tasks.len() != 6
        || !request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::QuarantineLauncherManagedTarget)
        || !request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::DownloadArtifactToTemp)
        || !request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::PromoteVerifiedArtifact)
    {
        return Some("guardian_artifact_repair_blocked_invalid_plan");
    }
    if request.source.url.trim().is_empty()
        || request.source.checksum_algorithm.trim().is_empty()
        || request.source.expected_checksum.trim().is_empty()
    {
        return Some("guardian_artifact_repair_blocked_missing_source");
    }
    let Some(checksum) = source_download_checksum(&request.source) else {
        return Some("guardian_artifact_repair_blocked_unsupported_checksum");
    };
    if !valid_download_checksum_metadata(checksum) {
        return Some("guardian_artifact_repair_blocked_invalid_checksum");
    }
    None
}

fn pre_missing_artifact_execution_block_reason(
    request: &GuardianArtifactRepairRequest<'_>,
) -> Option<&'static str> {
    if request.plan.tasks.len() != 5
        || request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::QuarantineLauncherManagedTarget)
        || !request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::DownloadArtifactToTemp)
        || !request
            .plan
            .tasks
            .iter()
            .any(|task| task.kind == GuardianRepairTaskKind::PromoteVerifiedArtifact)
    {
        return Some("guardian_missing_artifact_repair_blocked_invalid_plan");
    }
    match request.destination.try_exists() {
        Ok(false) => {}
        Ok(true) => return Some("guardian_missing_artifact_repair_blocked_target_exists"),
        Err(_) => return Some("guardian_missing_artifact_repair_blocked_target_unreadable"),
    }
    if request.source.url.trim().is_empty()
        || request.source.checksum_algorithm.trim().is_empty()
        || request.source.expected_checksum.trim().is_empty()
    {
        return Some("guardian_artifact_repair_blocked_missing_source");
    }
    let Some(checksum) = source_download_checksum(&request.source) else {
        return Some("guardian_artifact_repair_blocked_unsupported_checksum");
    };
    if !valid_download_checksum_metadata(checksum) {
        return Some("guardian_artifact_repair_blocked_invalid_checksum");
    }
    None
}

fn source_download_checksum<'a>(
    source: &GuardianArtifactRepairSource<'a>,
) -> Option<DownloadChecksum<'a>> {
    DownloadChecksumAlgorithm::parse(source.checksum_algorithm)
        .map(|algorithm| DownloadChecksum::new(algorithm, source.expected_checksum))
}

fn create_planned_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    plan: &GuardianRepairPlan,
) {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        plan.ownership,
        RollbackState::Available,
    );
    entry.targets.push(plan.target.clone());
    entry.planned_steps = plan
        .tasks
        .iter()
        .map(|task| {
            repair_step(
                &task.id,
                OperationStepResult::Planned,
                Some(task.target.clone()),
                Vec::new(),
                task_rollback(task.kind),
            )
        })
        .collect();
    entry
        .guardian_diagnosis_ids
        .push(safe_id(plan.diagnosis_id.as_str(), "diagnosis"));
    journals.create(entry);
}

fn create_terminal_journal(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    plan: &GuardianRepairPlan,
    status: OperationStatus,
    outcome: OperationOutcome,
    step_result: OperationStepResult,
    facts: Vec<String>,
) {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::RepairInstance,
        StabilizationSystem::Guardian,
        plan.ownership,
        RollbackState::Available,
    );
    entry.status = status;
    entry.targets.push(plan.target.clone());
    entry.planned_steps = plan
        .tasks
        .iter()
        .map(|task| {
            repair_step(
                &task.id,
                OperationStepResult::Planned,
                Some(task.target.clone()),
                Vec::new(),
                task_rollback(task.kind),
            )
        })
        .collect();
    entry.completed_steps.push(repair_step(
        "guardian_artifact_repair_blocked",
        step_result,
        Some(plan.target.clone()),
        facts,
        RollbackState::Available,
    ));
    entry
        .guardian_diagnosis_ids
        .push(safe_id(plan.diagnosis_id.as_str(), "diagnosis"));
    entry.outcome = Some(outcome);
    journals.create(entry);
}

fn repair_step(
    step_id: &str,
    result: OperationStepResult,
    target: Option<TargetDescriptor>,
    facts: Vec<String>,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step =
        OperationJournalStep::new(safe_id(step_id, "repair_step"), OperationPhase::Repairing);
    step.result = result;
    step.changed_target = target;
    step.generated_facts = facts;
    step.rollback = rollback;
    step
}

fn task_rollback(task: GuardianRepairTaskKind) -> RollbackState {
    match task {
        GuardianRepairTaskKind::QuarantineLauncherManagedTarget
        | GuardianRepairTaskKind::DownloadArtifactToTemp
        | GuardianRepairTaskKind::PromoteVerifiedArtifact => RollbackState::Available,
        GuardianRepairTaskKind::JournalRepairStart
        | GuardianRepairTaskKind::VerifyArtifactChecksum
        | GuardianRepairTaskKind::RecordRepairOutcome => RollbackState::NotApplicable,
    }
}

fn record_artifact_repair_memory(
    failure_memory: &GuardianFailureMemoryStore,
    diagnosis_id: &DiagnosisId,
    mode: GuardianMode,
    target: &TargetDescriptor,
    outcome: FailureMemoryActionOutcome,
    observed_at: &str,
    suppression_until: Option<&str>,
    repair_attempt: bool,
    quarantined_target: Option<TargetDescriptor>,
) {
    let mut entry = GuardianFailureMemoryEntry::observed(
        diagnosis_id.clone(),
        GuardianDomain::Install,
        target.clone(),
        mode,
        None,
        observed_at,
    )
    .with_action(GuardianActionKind::Repair, outcome);
    if repair_attempt {
        entry = entry.with_repair_attempt();
    }
    if let Some(suppression_until) = suppression_until {
        entry = entry.with_suppression_until(suppression_until);
    }
    if let Some(quarantined_target) = quarantined_target {
        entry = entry.with_quarantined_target(quarantined_target);
    }
    let _ = failure_memory.record(entry);
}

fn suppression_active(entry: &GuardianFailureMemoryEntry, now: &str) -> bool {
    let Some(suppression_until) = entry.suppression_until.as_deref() else {
        return false;
    };
    let Ok(suppression_until) = DateTime::parse_from_rfc3339(suppression_until) else {
        return false;
    };
    let Ok(now) = DateTime::<FixedOffset>::parse_from_rfc3339(now) else {
        return false;
    };
    suppression_until > now
}

fn default_suppression_until(observed_at: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(observed_at)
        .ok()
        .map(|observed_at| {
            (observed_at + Duration::minutes(DEFAULT_ARTIFACT_REPAIR_SUPPRESSION_MINUTES))
                .to_rfc3339()
        })
}

fn fact_ids(facts: &[ExecutionFact]) -> Vec<String> {
    facts
        .iter()
        .map(|fact| fact_id(fact.kind))
        .map(|fact| safe_id(fact, "execution_fact"))
        .collect()
}

fn fact_id(kind: ExecutionFactKind) -> &'static str {
    match kind {
        ExecutionFactKind::FileQuarantined => "file_quarantined",
        ExecutionFactKind::DownloadPromoted => "download_promoted",
        ExecutionFactKind::FilePromoted => "file_promoted",
        ExecutionFactKind::ArtifactVerified => "artifact_verified",
        ExecutionFactKind::DownloadChecksumMismatch => "download_checksum_mismatch",
        ExecutionFactKind::DownloadSizeMismatch => "download_size_mismatch",
        ExecutionFactKind::DownloadProviderFailure => "download_provider_failure",
        ExecutionFactKind::DownloadNetworkFailure => "download_network_failure",
        ExecutionFactKind::DownloadInterrupted => "download_interrupted",
        ExecutionFactKind::FileMissing => "file_missing",
        ExecutionFactKind::FilePermissionDenied => "file_permission_denied",
        ExecutionFactKind::PrimitiveRefused => "primitive_refused",
        _ => "execution_fact",
    }
}

fn artifact_repair_outcome(
    operation_id: OperationId,
    diagnosis_id: DiagnosisId,
    status: GuardianArtifactRepairStatus,
    facts: Vec<String>,
    summary: &str,
) -> GuardianArtifactRepairOutcome {
    GuardianArtifactRepairOutcome {
        operation_id: OperationId::new(safe_id(operation_id.as_str(), "operation")),
        diagnosis_id: DiagnosisId::new(safe_id(diagnosis_id.as_str(), "diagnosis")),
        action: GuardianActionKind::Repair,
        status,
        facts,
        summary: safe_id(summary, "guardian_artifact_repair"),
    }
}

fn safe_id(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn new_repair_operation_id() -> OperationId {
    OperationId::new(format!("guardian-artifact-repair:{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianArtifactRepairRequest, GuardianArtifactRepairSource, GuardianArtifactRepairStatus,
        execute_guardian_artifact_repair, execute_guardian_missing_artifact_repair,
    };
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianDecisionKind,
        GuardianMode, GuardianRepairPlanningContext, plan_launcher_managed_artifact_repair,
        plan_launcher_managed_missing_artifact_repair,
    };
    use crate::state::OperationJournalStore;
    use crate::state::contracts::{
        OperationId, OperationOutcome, OperationStatus, OwnershipClass, StabilizationSystem,
        TargetDescriptor, TargetKind,
    };
    use crate::state::failure_memory::{
        FailureMemoryActionOutcome, FailureMemoryKey, GuardianFailureMemoryEntry,
        GuardianFailureMemoryStore,
    };
    use reqwest::Client;
    use sha1::Sha1;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn repairs_launcher_managed_artifact_with_sha256_source() {
        let root = test_root("success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request(
            &plan,
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 1);
        assert!(outcome.facts.iter().any(|fact| fact == "download_promoted"));
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert_eq!(journal.outcome, Some(OperationOutcome::Succeeded));
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);
        assert!(memory[0].quarantined_target.is_some());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn repairs_launcher_managed_artifact_with_sha1_source() {
        let root = test_root("sha1-success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh minecraft artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request_with_checksum(
            &plan,
            &destination,
            &server.url,
            "sha1",
            &sha1_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert_eq!(server.request_count(), 1);
        assert!(outcome.facts.iter().any(|fact| fact == "download_promoted"));

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn repairs_missing_launcher_managed_artifact_without_quarantine() {
        let root = test_root("missing-success");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("missing.jar");
        let replacement = b"fresh missing artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let plan = missing_artifact_plan();

        let outcome = execute_guardian_missing_artifact_repair(request_with_checksum(
            &plan,
            &destination,
            &server.url,
            "sha1",
            &sha1_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Repaired);
        assert_eq!(fs::read(&destination).expect("replacement"), replacement);
        assert!(!root_contains_quarantine(&root, b"fresh missing artifact"));
        assert_eq!(server.request_count(), 1);
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Succeeded);
        assert!(
            !journal
                .completed_steps
                .iter()
                .any(|step| { step.step_id.contains("quarantine_launcher_managed_target") })
        );
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Repaired)
        );
        assert!(memory[0].quarantined_target.is_none());

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn checksum_failure_records_failed_without_repaired_status() {
        let root = test_root("checksum-failure");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request(
            &plan,
            &destination,
            &server.url,
            &sha256_hex(b"different artifact"),
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Failed);
        assert!(!destination.exists());
        assert!(root_contains_quarantine(&root, b"corrupt"));
        assert!(
            outcome
                .facts
                .iter()
                .any(|fact| fact == "download_checksum_mismatch")
        );
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Failed);
        assert_eq!(journal.outcome, Some(OperationOutcome::Failed));
        let memory = stores.failure_memory.list();
        assert_eq!(
            memory[0].last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(memory[0].repair_attempt_count, 1);

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn suppression_blocks_before_filesystem_or_network_mutation() {
        let root = test_root("suppressed");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement.clone());
        let stores = stores();
        let plan = artifact_plan();
        stores
            .failure_memory
            .record(
                GuardianFailureMemoryEntry::observed(
                    plan.diagnosis_id.clone(),
                    crate::guardian::GuardianDomain::Install,
                    plan.target.clone(),
                    GuardianMode::Managed,
                    None,
                    "2026-06-15T09:00:00Z",
                )
                .with_action(
                    crate::guardian::GuardianActionKind::Repair,
                    FailureMemoryActionOutcome::Failed,
                )
                .with_repair_attempt()
                .with_suppression_until("2026-06-15T10:30:00Z"),
            )
            .expect("memory");

        let outcome = execute_guardian_artifact_repair(request(
            &plan,
            &destination,
            &server.url,
            &sha256_hex(&replacement),
            Some(replacement.len() as u64),
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Suppressed);
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert_eq!(server.request_count(), 0);
        let key = FailureMemoryKey::for_observation(
            crate::guardian::GuardianDomain::Install,
            &plan.diagnosis_id,
            &plan.target,
            GuardianMode::Managed,
            None,
        );
        let memory = stores.failure_memory.get(&key).expect("memory");
        assert_eq!(
            memory.last_action_outcome,
            Some(FailureMemoryActionOutcome::Suppressed)
        );

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn missing_source_metadata_blocks_without_mutation() {
        let root = test_root("missing-source");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request(
            &plan,
            &destination,
            "",
            "",
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Blocked);
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        let journal = stores.journals.get(&outcome.operation_id).expect("journal");
        assert_eq!(journal.status, OperationStatus::Blocked);

        cleanup(&root);
    }

    #[tokio::test]
    async fn invalid_checksum_metadata_blocks_without_quarantine_or_network() {
        let root = test_root("invalid-checksum");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request_with_checksum(
            &plan,
            &destination,
            &server.url,
            "sha1",
            "-Xmx8192M",
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Blocked);
        assert_eq!(
            outcome.summary,
            "guardian_artifact_repair_blocked_invalid_checksum"
        );
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert_eq!(server.request_count(), 0);
        assert!(!root_contains_quarantine(&root, b"corrupt"));

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn unsupported_checksum_algorithm_blocks_without_quarantine_or_network() {
        let root = test_root("unsupported-checksum");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let replacement = b"fresh artifact".to_vec();
        let server = TestByteServer::start(replacement);
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request_with_checksum(
            &plan,
            &destination,
            &server.url,
            "sha512",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Blocked);
        assert_eq!(
            outcome.summary,
            "guardian_artifact_repair_blocked_unsupported_checksum"
        );
        assert_eq!(fs::read(&destination).expect("original"), b"corrupt");
        assert_eq!(server.request_count(), 0);
        assert!(!root_contains_quarantine(&root, b"corrupt"));

        server.stop();
        cleanup(&root);
    }

    #[tokio::test]
    async fn public_outcome_does_not_expose_source_or_paths() {
        let root = test_root("redaction");
        fs::create_dir_all(&root).expect("root");
        let destination = root.join("bad.jar");
        fs::write(&destination, b"corrupt").expect("corrupt artifact");
        let stores = stores();
        let plan = artifact_plan();

        let outcome = execute_guardian_artifact_repair(request(
            &plan,
            &destination,
            "https://example.invalid/artifact.jar?token=secret",
            "",
            None,
            &stores,
            "2026-06-15T10:00:00Z",
        ))
        .await;
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(outcome.status, GuardianArtifactRepairStatus::Blocked);
        assert!(!lower.contains("token"));
        assert!(!lower.contains("secret"));
        assert!(!lower.contains(root.to_string_lossy().as_ref()));

        cleanup(&root);
    }

    fn request<'a>(
        plan: &'a crate::guardian::GuardianRepairPlan,
        destination: &'a std::path::Path,
        url: &'a str,
        expected_sha256: &'a str,
        expected_size: Option<u64>,
        stores: &'a Stores,
        observed_at: &'a str,
    ) -> GuardianArtifactRepairRequest<'a> {
        request_with_checksum(
            plan,
            destination,
            url,
            "sha256",
            expected_sha256,
            expected_size,
            stores,
            observed_at,
        )
    }

    fn request_with_checksum<'a>(
        plan: &'a crate::guardian::GuardianRepairPlan,
        destination: &'a std::path::Path,
        url: &'a str,
        checksum_algorithm: &'a str,
        expected_checksum: &'a str,
        expected_size: Option<u64>,
        stores: &'a Stores,
        observed_at: &'a str,
    ) -> GuardianArtifactRepairRequest<'a> {
        GuardianArtifactRepairRequest {
            operation_id: Some(crate::state::contracts::OperationId::new(
                "guardian-artifact-repair-test",
            )),
            plan,
            destination,
            source: GuardianArtifactRepairSource {
                url,
                checksum_algorithm,
                expected_checksum,
                expected_size,
                max_bytes: Some(1024),
            },
            client: &stores.client,
            journals: &stores.journals,
            failure_memory: &stores.failure_memory,
            mode: GuardianMode::Managed,
            observed_at,
        }
    }

    fn artifact_plan() -> crate::guardian::GuardianRepairPlan {
        let decision = artifact_repair_decision();
        plan_launcher_managed_artifact_repair(
            &decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("plan")
    }

    fn missing_artifact_plan() -> crate::guardian::GuardianRepairPlan {
        let decision = artifact_repair_decision();
        plan_launcher_managed_missing_artifact_repair(
            &decision,
            GuardianRepairPlanningContext::current_operation(),
        )
        .expect("missing plan")
    }

    fn artifact_repair_decision() -> GuardianDecision {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "libraries_com_example_bad-1.0.jar",
            OwnershipClass::LauncherManaged,
        );
        GuardianDecision {
            operation_id: Some(OperationId::new("operation-install-repair")),
            mode: GuardianMode::Managed,
            kind: GuardianDecisionKind::Repair,
            diagnoses: vec![DiagnosisId::new("launcher_managed_artifact_corrupt")],
            action_plan: Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::new("launcher_managed_artifact_corrupt"),
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
                    reason: DiagnosisId::new("launcher_managed_artifact_corrupt"),
                }],
            )),
        }
    }

    struct Stores {
        journals: OperationJournalStore,
        failure_memory: GuardianFailureMemoryStore,
        client: Client,
    }

    fn stores() -> Stores {
        Stores {
            journals: OperationJournalStore::new(),
            failure_memory: GuardianFailureMemoryStore::new(),
            client: Client::new(),
        }
    }

    fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
        format!("{:x}", Sha256::digest(bytes.as_ref()))
    }

    fn sha1_hex(bytes: impl AsRef<[u8]>) -> String {
        format!("{:x}", Sha1::digest(bytes.as_ref()))
    }

    fn root_contains_quarantine(root: &std::path::Path, bytes: &[u8]) -> bool {
        fs::read_dir(root)
            .expect("read root")
            .filter_map(Result::ok)
            .any(|entry| {
                entry.file_name().to_string_lossy().contains(".quarantine-")
                    && fs::read(entry.path()).is_ok_and(|value| value == bytes)
            })
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-artifact-repair-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &PathBuf) {
        let _ = fs::remove_dir_all(root);
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
