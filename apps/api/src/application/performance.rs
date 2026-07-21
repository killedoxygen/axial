//! Performance-owned application workflows.
//!
//! Application orchestrates workflow requests here while `axial-performance`
//! owns rules refresh mechanics and validation.

mod benchmark_matrix;
mod host;
mod qualification;
mod workflow;

use crate::guardian::{GuardianFact, performance_rules_guardian_facts};
use crate::observability::{
    RedactionAudience, bounded_descriptor_token, evidence_text_looks_sensitive,
    sanitize_evidence_token,
};
use crate::state::{
    AppState, OperationJournalReconciliation, OperationJournalStoreError, RequestProducerHandoff,
    contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
        RollbackState, StabilizationSystem, TargetDescriptor,
    },
    operation_journal_completed_step_is_visible, operation_journal_plan_is_visible,
    ownership::{CurrentArtifact, classify_current_artifact},
};
use axial_performance::{
    BundleHealth, CompositionPlan, CompositionTier, PerformanceMode, PerformanceRulesStatus,
    RuleChannel, RuleSource, RulesRefreshError, RulesValidation,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;

const RULES_REFRESH_RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);
const RULES_JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(20);
const RULES_JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

enum RulesJournalReconciliation {
    MutationCommitted,
    AcceptedFailure(OperationJournalStoreError),
    RetryMutation,
}

struct RulesRefreshRequestGuard {
    abandoned: Arc<AtomicBool>,
    armed: bool,
}

impl RulesRefreshRequestGuard {
    fn new(abandoned: Arc<AtomicBool>) -> Self {
        Self {
            abandoned,
            armed: true,
        }
    }

    fn finish(mut self) {
        self.armed = false;
    }

    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
    }
}

impl Drop for RulesRefreshRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.abandon();
        }
    }
}

pub(crate) use benchmark_matrix::{
    BenchmarkMatrix, BenchmarkSuiteRunSpec, benchmark_matrix, benchmark_suite_manifest_run_inputs,
    benchmark_suite_plan, benchmark_suite_run_descriptor, benchmark_suite_run_id,
};
pub use host::{SystemResourceResponse, system_resource_status};
#[cfg(test)]
pub(crate) use qualification::{
    FAMILY_C_BASELINE_TARGET_ID, FAMILY_C_MANAGED_COMPOSITION_ID, FAMILY_C_MANAGED_TARGET_ID,
    FAMILY_C_QUALIFICATION_VERSION,
};
pub(crate) use qualification::{
    FAMILY_C_QUALIFICATION_MODE, family_c_qualification_payload,
    family_c_qualification_preview_payload,
};
pub use workflow::{
    PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstallRequest,
    PerformanceInstallResponse, PerformanceInstanceDisplay, PerformanceInstanceOperationResponse,
    PerformanceManagedArtifactSummary, PerformanceMemoryDisplay, PerformanceModeDisplay,
    PerformanceOperationStatusResponse, PerformancePlanRequest, PerformancePlanResponse,
    PerformanceRollbackListRequest, PerformanceRollbackListResponse, PerformanceRuntimeDisplay,
    performance_instance_operation, performance_operation_status, performance_plan,
    performance_rollback_list,
};
pub(crate) use workflow::{
    performance_health, performance_install, spawn_pending_performance_operations,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewModelTone {
    Ok,
    Warn,
    Err,
    Mute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewModelAction {
    pub command: CommandKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformancePlanSummaryViewModel {
    pub state_id: String,
    pub title: String,
    pub detail: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition_id: Option<String>,
    #[serde(default)]
    pub managed_artifact_count: usize,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Debug, Serialize)]
pub struct PerformanceRulesStatusResponse {
    #[serde(flatten)]
    pub status: PerformanceRulesStatus,
    pub view_model: PerformanceRulesStatusViewModel,
    pub guardian_facts: Vec<GuardianFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PerformanceRulesStatusViewModel {
    pub source_label: String,
    pub channel_label: String,
    pub validation_label: String,
    pub validation_tone: ViewModelTone,
    pub validation_icon: String,
    pub summary: String,
    pub refresh_label: String,
    pub generated_label: String,
    pub cache_label: String,
    pub emergency_disable_label: String,
    pub details_label: String,
    pub health_states_label: String,
    pub ownership_label: String,
    pub warnings: Vec<String>,
}

pub fn performance_plan_summary_view_model(
    mode: PerformanceMode,
    plan: Option<&CompositionPlan>,
    health: BundleHealth,
    rollback: RollbackState,
    managed_artifact_count: usize,
    warnings: &[String],
) -> PerformancePlanSummaryViewModel {
    let plan_available = plan.is_some();
    if !matches!(mode, PerformanceMode::Managed) || matches!(health, BundleHealth::Disabled) {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_disabled".to_string(),
            title: "No managed bundle".to_string(),
            detail: "Memory allocation and Java detection are shown below.".to_string(),
            tone: ViewModelTone::Mute,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: plan
                .map(|plan| public_performance_descriptor(&plan.composition_id, "composition")),
            managed_artifact_count,
            actions: Vec::new(),
        };
    }

    if matches!(health, BundleHealth::Invalid) && plan.is_none() {
        let warning = warnings
            .first()
            .and_then(|warning| public_performance_notice(warning, false));
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_invalid".to_string(),
            title: "Bundle needs attention".to_string(),
            detail: warning
                .unwrap_or_else(|| "Managed performance state needs attention.".to_string()),
            tone: ViewModelTone::Err,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: None,
            managed_artifact_count,
            actions: performance_summary_actions(mode, health, rollback, plan_available),
        };
    }

    let Some(plan) = plan else {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_unavailable".to_string(),
            title: "Bundle status unavailable".to_string(),
            detail: "Plan details are unavailable.".to_string(),
            tone: ViewModelTone::Mute,
            health: Some(bundle_health_token(health).to_string()),
            composition_id: None,
            managed_artifact_count,
            actions: performance_summary_actions(mode, health, rollback, plan_available),
        };
    };

    let tier = composition_tier_label(plan.tier);
    let mod_count = plan.mods.len();
    let fallback = !plan.fallback_reason.trim().is_empty();
    let warning = plan
        .fallback_reason
        .trim()
        .split_once('\n')
        .map(|(first, _)| first)
        .unwrap_or_else(|| plan.fallback_reason.trim())
        .to_string();
    let warning = public_performance_notice(
        first_nonempty([
            warning.as_str(),
            warnings.first().map(String::as_str).unwrap_or_default(),
            plan.warnings
                .first()
                .map(String::as_str)
                .unwrap_or_default(),
        ]),
        fallback,
    );
    let launcher_tuning = matches!(plan.tier, CompositionTier::VanillaEnhanced) || mod_count == 0;

    if launcher_tuning {
        return PerformancePlanSummaryViewModel {
            state_id: "performance_summary_launcher_tuning".to_string(),
            title: "Launcher tuning".to_string(),
            detail: warning.unwrap_or_else(|| {
                "Axial will tune Java and memory for this version; no performance mod bundle is available."
                    .to_string()
            }),
            tone: if fallback {
                ViewModelTone::Warn
            } else {
                health_view_tone(health)
            },
            health: Some(bundle_health_token(health).to_string()),
            composition_id: Some(public_performance_descriptor(
                &plan.composition_id,
                "composition",
            )),
            managed_artifact_count,
            actions: performance_summary_actions(mode, health, rollback, plan_available),
        };
    }

    let health_text = format!("bundle {}", health_label(health));
    PerformancePlanSummaryViewModel {
        state_id: format!("performance_summary_{}", bundle_health_token(health)),
        title: tier,
        detail: warning.unwrap_or_else(|| {
            format!(
                "{} performance mod{} selected; {}.",
                mod_count,
                if mod_count == 1 { "" } else { "s" },
                health_text
            )
        }),
        tone: if fallback {
            ViewModelTone::Warn
        } else {
            health_view_tone(health)
        },
        health: Some(bundle_health_token(health).to_string()),
        composition_id: Some(public_performance_descriptor(
            &plan.composition_id,
            "composition",
        )),
        managed_artifact_count,
        actions: performance_summary_actions(mode, health, rollback, plan_available),
    }
}

pub(super) fn public_performance_descriptor(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_string()
    } else {
        bounded_descriptor_token(value, fallback)
    }
}

fn performance_summary_actions(
    mode: PerformanceMode,
    health: BundleHealth,
    rollback: RollbackState,
    plan_available: bool,
) -> Vec<ViewModelAction> {
    if !matches!(mode, PerformanceMode::Managed) || matches!(health, BundleHealth::Disabled) {
        return Vec::new();
    }

    let install_disabled_reason = (!plan_available).then(|| {
        if matches!(health, BundleHealth::Invalid) {
            "Managed performance state needs repair before this action can run.".to_string()
        } else {
            "The managed performance plan is unavailable.".to_string()
        }
    });
    let install_label = match health {
        BundleHealth::Healthy => "Reapply managed bundle",
        BundleHealth::Invalid => "Repair managed bundle",
        BundleHealth::Disabled => "Apply managed bundle",
    };
    let mut actions = vec![performance_view_action(
        "install",
        install_label,
        install_disabled_reason.is_none(),
        install_disabled_reason,
    )];

    let rollback_enabled = matches!(rollback, RollbackState::Available);
    actions.push(performance_view_action(
        "rollback",
        "Rollback managed bundle",
        rollback_enabled,
        (!rollback_enabled).then(|| "No rollback snapshot is available.".to_string()),
    ));

    actions
}

fn performance_view_action(
    action: &str,
    label: &str,
    enabled: bool,
    disabled_reason: Option<String>,
) -> ViewModelAction {
    ViewModelAction {
        command: CommandKind::ApplyPerformancePlan,
        action: Some(action.to_string()),
        label: label.to_string(),
        enabled,
        disabled_reason,
    }
}

fn composition_tier_label(tier: CompositionTier) -> String {
    match tier {
        CompositionTier::Extended => "Full mod bundle",
        CompositionTier::Core => "Core mod bundle",
        CompositionTier::VanillaEnhanced => "Launcher tuning",
    }
    .to_string()
}

fn health_label(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Invalid => "needs attention",
        BundleHealth::Disabled => "not installed",
    }
}

fn health_view_tone(health: BundleHealth) -> ViewModelTone {
    match health {
        BundleHealth::Healthy => ViewModelTone::Ok,
        BundleHealth::Disabled => ViewModelTone::Warn,
        BundleHealth::Invalid => ViewModelTone::Err,
    }
}

fn bundle_health_token(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Disabled => "disabled",
        BundleHealth::Invalid => "invalid",
    }
}

fn public_performance_notice(raw: &str, fallback: bool) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || evidence_text_looks_sensitive(raw) {
        return None;
    }
    let notice = if raw.contains("temporarily unavailable")
        || raw.contains("not compatible with this instance")
    {
        raw.to_string()
    } else if raw.contains("skipped by emergency disable") {
        if fallback {
            "A performance bundle is temporarily unavailable, so Axial chose the safest available option."
                .to_string()
        } else {
            "One performance mod is temporarily unavailable, so Axial left it out.".to_string()
        }
    } else if raw.contains("not enough compatible performance mods") {
        "A faster performance bundle is not compatible with this instance, so Axial chose a safer option."
            .to_string()
    } else if raw.contains("no NVIDIA Turing+ GPU detected") {
        "Nvidium was left out because this device does not have a supported NVIDIA GPU.".to_string()
    } else {
        raw.replace("managed mod", "installed mod")
    };
    Some(notice)
}

fn first_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> &'a str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

pub fn performance_rules_status(state: &AppState) -> PerformanceRulesStatusResponse {
    performance_rules_status_response(state.performance().rules_status())
}

pub(crate) async fn refresh_performance_rules(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<PerformanceRulesStatusResponse, RefreshPerformanceRulesError> {
    let producer = handoff
        .try_claim()
        .map_err(|_| RefreshPerformanceRulesError::ShuttingDown)?;
    let state = state.clone();
    let abandoned = Arc::new(AtomicBool::new(false));
    let request_guard = RulesRefreshRequestGuard::new(abandoned.clone());
    let terminal_failure = Arc::new(tokio::sync::Notify::new());
    let terminal_failure_task = terminal_failure.clone();
    let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.spawn(async move {
        let result = handle_refresh_performance_rules(
            &state,
            abandoned.as_ref(),
            ready_tx,
            terminal_failure_task,
        )
        .await;
        let _ = result_tx.send(result);
    });
    let mut result_rx = result_rx;
    tokio::select! {
        result = &mut result_rx => {
            request_guard.finish();
            result.unwrap_or_else(|_| {
                Err(RefreshPerformanceRulesError::Journal(
                    OperationJournalStoreError::Persistence(std::io::Error::other(
                        "performance-rules refresh owner stopped before responding",
                    )),
                ))
            })
        }
        ready = &mut ready_rx => {
            if ready.is_err() {
                request_guard.finish();
                result_rx.await.unwrap_or_else(|_| {
                    Err(RefreshPerformanceRulesError::Journal(
                        OperationJournalStoreError::Persistence(std::io::Error::other(
                            "performance-rules refresh owner stopped before effect ownership",
                        )),
                    ))
                })
            } else {
                request_guard.finish();
                tokio::select! {
                    result = &mut result_rx => result.unwrap_or_else(|_| {
                        Err(RefreshPerformanceRulesError::Journal(
                            OperationJournalStoreError::Persistence(std::io::Error::other(
                                "performance-rules refresh owner stopped before responding",
                            )),
                        ))
                    }),
                    () = terminal_failure.notified() => Err(RefreshPerformanceRulesError::Journal(
                        OperationJournalStoreError::Persistence(std::io::Error::other(
                            "performance-rules terminal journal reconciliation is still pending",
                        )),
                    )),
                }
            }
        }
        () = tokio::time::sleep(RULES_REFRESH_RESPONSE_TIMEOUT) => {
            request_guard.abandon();
            Err(RefreshPerformanceRulesError::Journal(
                OperationJournalStoreError::Persistence(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "performance-rules journal reconciliation is still pending",
                )),
            ))
        }
    }
}

pub fn refresh_performance_rules_error_response(
    error: RefreshPerformanceRulesError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        RefreshPerformanceRulesError::Unconfigured => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "performance remote rules url is not configured"
            })),
        ),
        RefreshPerformanceRulesError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "performance operations are shutting down"
            })),
        ),
        RefreshPerformanceRulesError::Refresh(
            RulesRefreshError::Request(_)
            | RulesRefreshError::HttpStatus(_)
            | RulesRefreshError::ResponseTooLarge
            | RulesRefreshError::Parse(_)
            | RulesRefreshError::Validation(_)
            | RulesRefreshError::Signature(_),
        ) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "Performance rules provider response could not be verified. Try again later."
            })),
        ),
        _error => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Could not load performance data. Check app data permissions and try again."
            })),
        ),
    }
}

async fn handle_refresh_performance_rules(
    state: &AppState,
    abandoned: &AtomicBool,
    ready_for_effect: tokio::sync::oneshot::Sender<()>,
    terminal_failure: Arc<tokio::sync::Notify>,
) -> Result<PerformanceRulesStatusResponse, RefreshPerformanceRulesError> {
    let (operation_id, recovered_error) =
        begin_fresh_rules_journal_with_mint(state, new_refresh_rules_operation_id).await?;
    if let Some(error) = recovered_error {
        terminalize_recovered_rules_journal(state, &operation_id).await?;
        return Err(error.into());
    }
    if abandoned.load(Ordering::Acquire) {
        terminalize_recovered_rules_journal(state, &operation_id).await?;
        return Err(
            OperationJournalStoreError::Persistence(std::io::Error::other(
                "performance-rules request ended before journal reconciliation",
            ))
            .into(),
        );
    }
    if ready_for_effect.send(()).is_err() {
        terminalize_recovered_rules_journal(state, &operation_id).await?;
        return Err(
            OperationJournalStoreError::Persistence(std::io::Error::other(
                "performance-rules request ended before effect ownership",
            ))
            .into(),
        );
    }

    let before = state.performance().rules_status();
    match state.refresh_performance_rules().await {
        Ok(status) => {
            let cache_changed = performance_rules_cache_changed(&before, &status);
            if let Some(error) = record_rules_terminal_reconciled(
                state,
                &operation_id,
                refresh_rules_step_with_cache_change(OperationStepResult::Completed, cache_changed),
                None,
                Some(terminal_failure.as_ref()),
            )
            .await?
            {
                return Err(error.into());
            }
            Ok(performance_rules_status_response(status))
        }
        Err(error) => {
            if let Some(journal_error) = record_rules_terminal_reconciled(
                state,
                &operation_id,
                refresh_rules_step(OperationStepResult::Failed),
                Some("refresh_remote_rules"),
                Some(terminal_failure.as_ref()),
            )
            .await?
            {
                return Err(journal_error.into());
            }
            Err(RefreshPerformanceRulesError::from(error))
        }
    }
}

const RULES_OPERATION_ID_MINT_ATTEMPTS: usize = 8;

async fn begin_fresh_rules_journal_with_mint<Mint>(
    state: &AppState,
    mut mint: Mint,
) -> Result<(OperationId, Option<OperationJournalStoreError>), OperationJournalStoreError>
where
    Mint: FnMut() -> OperationId,
{
    for _ in 0..RULES_OPERATION_ID_MINT_ATTEMPTS {
        let operation_id = mint();
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{operation_id}")),
            operation_id.clone(),
            CommandKind::RefreshPerformanceRules,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry
            .planned_steps
            .push(refresh_rules_step(OperationStepResult::Planned));
        entry.targets = refresh_rules_targets();
        match create_rules_journal_reconciled(state, entry).await {
            Ok(recovered_error) => return Ok((operation_id, recovered_error)),
            Err(OperationJournalStoreError::AlreadyExists) => {}
            Err(error) => return Err(error),
        }
    }
    Err(OperationJournalStoreError::AlreadyExists)
}

async fn create_rules_journal_reconciled(
    state: &AppState,
    entry: OperationJournalEntry,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        match state.journals().create_fresh(entry.clone()).await {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyExists) => {
                return Err(OperationJournalStoreError::AlreadyExists);
            }
            Err(OperationJournalStoreError::RetryRequired) => {
                state.journals().retry().await?;
            }
            Err(error) => {
                match reconcile_rules_journal_error(state, &entry.operation_id, error, |current| {
                    operation_journal_plan_is_visible(current, &entry)
                })
                .await?
                {
                    RulesJournalReconciliation::MutationCommitted => return Ok(None),
                    RulesJournalReconciliation::AcceptedFailure(error) => return Ok(Some(error)),
                    RulesJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn record_rules_terminal_reconciled(
    state: &AppState,
    operation_id: &OperationId,
    step: OperationJournalStep,
    failure_point: Option<&str>,
    terminal_failure: Option<&tokio::sync::Notify>,
) -> Result<Option<OperationJournalStoreError>, OperationJournalStoreError> {
    loop {
        let result = if let Some(failure_point) = failure_point {
            state
                .journals()
                .record_failure(
                    operation_id,
                    step.clone(),
                    failure_point,
                    OperationOutcome::Failed,
                )
                .await
        } else {
            state
                .journals()
                .record_success(operation_id, step.clone(), OperationOutcome::Succeeded)
                .await
        };
        match result {
            Ok(()) => return Ok(None),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if state.journals().get(operation_id).is_some_and(|entry| {
                    rules_terminal_transition_matches(&entry, operation_id, failure_point, &step)
                }) =>
            {
                return Ok(None);
            }
            Err(error) => {
                if matches!(error, OperationJournalStoreError::Persistence(_))
                    && let Some(terminal_failure) = terminal_failure
                {
                    terminal_failure.notify_one();
                }
                match reconcile_rules_journal_error(state, operation_id, error, |entry| {
                    rules_terminal_transition_matches(entry, operation_id, failure_point, &step)
                })
                .await?
                {
                    RulesJournalReconciliation::MutationCommitted => return Ok(None),
                    RulesJournalReconciliation::AcceptedFailure(error) => return Ok(Some(error)),
                    RulesJournalReconciliation::RetryMutation => {}
                }
            }
        }
    }
}

async fn terminalize_recovered_rules_journal(
    state: &AppState,
    operation_id: &OperationId,
) -> Result<(), OperationJournalStoreError> {
    record_rules_terminal_reconciled(
        state,
        operation_id,
        refresh_rules_step(OperationStepResult::Failed),
        Some("refresh_rules_journal_reconciliation"),
        None,
    )
    .await
    .map(|_| ())
}

async fn reconcile_rules_journal_error(
    state: &AppState,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<RulesJournalReconciliation, OperationJournalStoreError> {
    match state
        .journals()
        .reconcile_transition(
            operation_id,
            error,
            RULES_JOURNAL_RETRY_INITIAL_DELAY,
            RULES_JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(error) => {
            Ok(RulesJournalReconciliation::AcceptedFailure(error))
        }
        OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(RulesJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(RulesJournalReconciliation::RetryMutation)
        }
    }
}

fn rules_terminal_transition_matches(
    entry: &OperationJournalEntry,
    operation_id: &OperationId,
    failure_point: Option<&str>,
    step: &OperationJournalStep,
) -> bool {
    let (status, outcome) = if failure_point.is_some() {
        (OperationStatus::Failed, OperationOutcome::Failed)
    } else {
        (OperationStatus::Succeeded, OperationOutcome::Succeeded)
    };
    &entry.operation_id == operation_id
        && entry.command == CommandKind::RefreshPerformanceRules
        && entry.owner == StabilizationSystem::Application
        && entry.ownership == OwnershipClass::LauncherManaged
        && entry.targets == refresh_rules_targets()
        && entry.status == status
        && entry.outcome == Some(outcome)
        && entry.failure_point.as_deref() == failure_point
        && operation_journal_completed_step_is_visible(entry, step)
}

fn performance_rules_status_response(
    mut status: PerformanceRulesStatus,
) -> PerformanceRulesStatusResponse {
    sanitize_performance_rules_status(&mut status);
    let guardian_facts = performance_rules_guardian_facts(&status, OperationPhase::Validating);
    let view_model = performance_rules_status_view_model(&status);
    PerformanceRulesStatusResponse {
        status,
        view_model,
        guardian_facts,
    }
}

fn sanitize_performance_rules_status(status: &mut PerformanceRulesStatus) {
    status.warnings = public_rules_warnings(&status.warnings);
    status.rules_cache.warning = status
        .rules_cache
        .warning
        .as_deref()
        .and_then(public_rules_warning);
    for coverage in &mut status.family_coverage {
        coverage.warnings = public_rules_warnings(&coverage.warnings);
    }
}

fn public_rules_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .filter_map(|warning| public_rules_warning(warning))
        .collect()
}

fn public_rules_warning(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else if evidence_text_looks_sensitive(raw) {
        Some("Performance rule diagnostics are unavailable.".to_string())
    } else {
        Some(raw.to_string())
    }
}

fn performance_rules_status_view_model(
    status: &PerformanceRulesStatus,
) -> PerformanceRulesStatusViewModel {
    let validation_valid = status.validation == RulesValidation::Valid;
    let warnings = public_rules_warnings(&status.warnings);
    PerformanceRulesStatusViewModel {
        source_label: performance_rule_source_label(status.rule_source).to_string(),
        channel_label: performance_rule_channel_label(status.rule_channel).to_string(),
        validation_label: if validation_valid { "Valid" } else { "Invalid" }.to_string(),
        validation_tone: if validation_valid {
            ViewModelTone::Ok
        } else {
            ViewModelTone::Err
        },
        validation_icon: if validation_valid { "check" } else { "alert" }.to_string(),
        summary: if validation_valid {
            "Managed performance defaults are ready.".to_string()
        } else {
            "Managed performance rules need attention.".to_string()
        },
        refresh_label: performance_rules_refresh_label(status),
        generated_label: public_performance_timestamp(&status.generated_at, "generated_at"),
        cache_label: performance_rules_cache_label(status).to_string(),
        emergency_disable_label: performance_rules_emergency_disable_label(status),
        details_label: performance_rules_details_label(warnings.len()),
        health_states_label: status
            .health_states
            .iter()
            .map(|health| performance_rules_health_label(*health))
            .collect::<Vec<_>>()
            .join(", "),
        ownership_label: status
            .ownership_classes
            .iter()
            .map(|ownership| performance_rules_ownership_label(*ownership))
            .collect::<Vec<_>>()
            .join(", "),
        warnings,
    }
}

fn performance_rule_source_label(source: RuleSource) -> &'static str {
    match source {
        RuleSource::BuiltIn => "Built-in rules",
        RuleSource::Remote => "Remote rules",
    }
}

fn performance_rule_channel_label(channel: RuleChannel) -> &'static str {
    match channel {
        RuleChannel::Bundled => "Bundled manifest",
        RuleChannel::Local => "Local cache",
        RuleChannel::Remote => "Remote manifest",
    }
}

fn performance_rules_refresh_label(status: &PerformanceRulesStatus) -> String {
    if !status.remote_refresh {
        return "Remote refresh off".to_string();
    }
    status
        .last_refresh_at
        .as_deref()
        .map(|value| {
            format!(
                "Last refreshed {}",
                public_performance_timestamp(value, "refresh_time")
            )
        })
        .unwrap_or_else(|| "Remote refresh configured, not refreshed yet".to_string())
}

fn public_performance_timestamp(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| fallback.to_string())
}

fn performance_rules_cache_label(status: &PerformanceRulesStatus) -> &'static str {
    if status.rules_cache.state == axial_performance::RulesCacheState::Invalid {
        "Invalid local cache"
    } else if !status.rules_cache.recorded {
        "Unavailable"
    } else {
        "Recorded locally"
    }
}

fn performance_rules_emergency_disable_label(status: &PerformanceRulesStatus) -> String {
    let count = status
        .emergency_disable_count
        .max(status.emergency_disables.len());
    if count == 0 {
        return "None active".to_string();
    }
    let prefix = format!("{count} active");
    let Some(reason) = status
        .emergency_disables
        .first()
        .and_then(|disable| public_rules_warning(&disable.reason))
    else {
        return prefix;
    };
    format!("{prefix}: {reason}")
}

fn performance_rules_details_label(warning_count: usize) -> String {
    if warning_count == 0 {
        "Rule details".to_string()
    } else {
        format!(
            "Rule details, {warning_count} warning{}",
            if warning_count == 1 { "" } else { "s" }
        )
    }
}

fn performance_rules_health_label(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "Healthy",
        BundleHealth::Disabled => "Disabled",
        BundleHealth::Invalid => "Invalid",
    }
}

fn performance_rules_ownership_label(ownership: axial_performance::OwnershipClass) -> &'static str {
    match ownership {
        axial_performance::OwnershipClass::CompositionManaged => "Axial-managed",
        axial_performance::OwnershipClass::UserManaged => "User-managed",
    }
}

fn new_refresh_rules_operation_id() -> OperationId {
    OperationId::mint()
}

fn refresh_rules_step(result: OperationStepResult) -> OperationJournalStep {
    refresh_rules_step_with_cache_change(result, result == OperationStepResult::Completed)
}

fn refresh_rules_step_with_cache_change(
    result: OperationStepResult,
    cache_changed: bool,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new("refresh_remote_rules", OperationPhase::Running);
    step.result = result;
    if result == OperationStepResult::Completed && cache_changed {
        step.changed_target = Some(
            classify_current_artifact(
                CurrentArtifact::PerformanceRulesCache,
                "performance_rules_cache",
            )
            .target,
        );
    }
    step
}

fn performance_rules_cache_changed(
    before: &PerformanceRulesStatus,
    after: &PerformanceRulesStatus,
) -> bool {
    before.rule_source != after.rule_source
        || before.rule_channel != after.rule_channel
        || before.generated_at != after.generated_at
        || before.last_refresh_at != after.last_refresh_at
}

fn refresh_rules_targets() -> Vec<TargetDescriptor> {
    vec![
        classify_current_artifact(
            CurrentArtifact::ExternalPerformanceRules,
            "performance_rules_remote_source",
        )
        .target,
        classify_current_artifact(
            CurrentArtifact::PerformanceRulesCache,
            "performance_rules_cache",
        )
        .target,
    ]
}

#[derive(Debug, Error)]
pub enum RefreshPerformanceRulesError {
    #[error("performance remote rules url is not configured")]
    Unconfigured,
    #[error("performance operations are shutting down")]
    ShuttingDown,
    #[error(transparent)]
    Refresh(RulesRefreshError),
    #[error(transparent)]
    Journal(#[from] OperationJournalStoreError),
}

impl From<RulesRefreshError> for RefreshPerformanceRulesError {
    fn from(error: RulesRefreshError) -> Self {
        match error {
            RulesRefreshError::Unconfigured => Self::Unconfigured,
            error => Self::Refresh(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ViewModelTone, performance_plan_summary_view_model, performance_rules_status_response,
    };
    use crate::state::contracts::RollbackState;
    use axial_performance::{
        BundleHealth, CompositionPlan, CompositionTier, ModCondition, PerformanceMode, RuleChannel,
        RuleSource, RulesCacheState, RulesCacheStatus, RulesValidation, builtin_manifest,
        rules_status_for,
        types::{ManagedMod, VersionFamily},
    };

    #[test]
    fn performance_summary_view_model_matches_managed_healthy_meaning() {
        let plan = test_plan(
            "core",
            CompositionTier::Core,
            vec![test_mod("sodium", "Sodium")],
            "",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Healthy,
            RollbackState::Unavailable,
            1,
            &[],
        );

        assert_eq!(summary.state_id, "performance_summary_healthy");
        assert_eq!(summary.title, "Core mod bundle");
        assert_eq!(
            summary.detail,
            "1 performance mod selected; bundle healthy."
        );
        assert_eq!(summary.tone, ViewModelTone::Ok);
        assert_eq!(summary.managed_artifact_count, 1);
        assert!(summary.actions.iter().any(|action| {
            action.action.as_deref() == Some("install")
                && action.label == "Reapply managed bundle"
                && action.enabled
        }));
        assert!(summary.actions.iter().any(|action| {
            action.action.as_deref() == Some("rollback")
                && !action.enabled
                && action.disabled_reason.as_deref() == Some("No rollback snapshot is available.")
        }));
    }

    #[test]
    fn performance_summary_view_model_allows_repair_with_a_resolved_plan() {
        let plan = test_plan(
            "core",
            CompositionTier::Core,
            vec![test_mod("sodium", "Sodium")],
            "",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Invalid,
            RollbackState::Unavailable,
            1,
            &[],
        );

        assert!(summary.actions.iter().any(|action| {
            action.action.as_deref() == Some("install")
                && action.label == "Repair managed bundle"
                && action.enabled
                && action.disabled_reason.is_none()
        }));
    }

    #[test]
    fn performance_summary_view_model_normalizes_fallback_warning() {
        let plan = test_plan(
            "fallback",
            CompositionTier::VanillaEnhanced,
            Vec::new(),
            "not enough compatible performance mods for preferred composition",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Healthy,
            RollbackState::Available,
            0,
            &[],
        );

        assert_eq!(summary.state_id, "performance_summary_launcher_tuning");
        assert_eq!(summary.title, "Launcher tuning");
        assert_eq!(
            summary.detail,
            "A faster performance bundle is not compatible with this instance, so Axial chose a safer option."
        );
        assert_eq!(summary.tone, ViewModelTone::Warn);
        assert!(
            summary
                .actions
                .iter()
                .any(|action| { action.action.as_deref() == Some("rollback") && action.enabled })
        );
    }

    #[test]
    fn performance_summary_view_model_drops_sensitive_warning_material() {
        let warnings = vec![
            r"managed.jar missing from C:\Users\Alice\.minecraft\mods with -Xmx8192M".to_string(),
        ];

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            None,
            BundleHealth::Invalid,
            RollbackState::Unavailable,
            0,
            &warnings,
        );

        assert_eq!(summary.title, "Bundle needs attention");
        assert_eq!(summary.detail, "Managed performance state needs attention.");
        assert!(!summary.detail.contains("Alice"));
        assert!(!summary.detail.contains("-Xmx"));
        assert!(summary.actions.iter().any(|action| {
            action.action.as_deref() == Some("install")
                && !action.enabled
                && action.disabled_reason.as_deref()
                    == Some("Managed performance state needs repair before this action can run.")
        }));
    }

    #[test]
    fn performance_summary_view_model_bounds_public_composition_id() {
        let raw_composition_id = r"C:\Users\Alice\.minecraft\mods\secret.jar";
        let plan = test_plan(
            raw_composition_id,
            CompositionTier::Core,
            vec![test_mod("sodium", "Sodium")],
            "",
            Vec::new(),
        );

        let summary = performance_plan_summary_view_model(
            PerformanceMode::Managed,
            Some(&plan),
            BundleHealth::Healthy,
            RollbackState::Available,
            1,
            &[],
        );
        let encoded = serde_json::to_string(&summary).expect("serialize summary");

        assert_ne!(summary.composition_id.as_deref(), Some(raw_composition_id));
        assert!(
            summary
                .composition_id
                .as_deref()
                .is_some_and(|value| value.starts_with("composition-"))
        );
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains(".minecraft"));
    }

    #[test]
    fn performance_rules_status_response_authors_invalid_rules_guardian_fact() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let mut cache = RulesCacheStatus::unavailable();
        cache.state = RulesCacheState::Invalid;
        cache.warning = Some("missing signature".to_string());
        let status = rules_status_for(
            &manifest,
            RuleSource::Remote,
            RuleChannel::Remote,
            cache,
            true,
            None,
            RulesValidation::Invalid,
        );

        let response = performance_rules_status_response(status);

        assert_eq!(response.status.validation, RulesValidation::Invalid);
        assert_eq!(response.view_model.validation_label, "Invalid");
        assert_eq!(response.view_model.validation_tone, ViewModelTone::Err);
        assert_eq!(
            response.view_model.summary,
            "Managed performance rules need attention."
        );
        assert_eq!(response.view_model.cache_label, "Invalid local cache");
        let fact = response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_rules_invalid")
            .expect("invalid rules Guardian fact");
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("performance_rules_remote_source")
        );
    }

    #[test]
    fn performance_rules_status_response_bounds_public_warnings_and_view_model_copy() {
        let manifest = builtin_manifest().expect("builtin manifest");
        let mut cache = RulesCacheStatus::unavailable();
        cache.state = RulesCacheState::Invalid;
        cache.warning =
            Some(r"C:\Users\Alice\.minecraft\performance.json?api_token=secret".to_string());
        let status = rules_status_for(
            &manifest,
            RuleSource::Remote,
            RuleChannel::Remote,
            cache,
            true,
            Some("2026-06-15T12:00:00Z".to_string()),
            RulesValidation::Invalid,
        );

        let response = performance_rules_status_response(status);
        let encoded = serde_json::to_string(&response).expect("serialize status");

        assert_eq!(
            response.status.warnings,
            vec!["Performance rule diagnostics are unavailable.".to_string()]
        );
        assert_eq!(
            response.status.rules_cache.warning.as_deref(),
            Some("Performance rule diagnostics are unavailable.")
        );
        assert_eq!(
            response.view_model.warnings,
            vec!["Performance rule diagnostics are unavailable.".to_string()]
        );
        assert_eq!(response.view_model.source_label, "Remote rules");
        assert_eq!(response.view_model.channel_label, "Remote manifest");
        assert_eq!(
            response.view_model.refresh_label,
            "Last refreshed 2026-06-15T12:00:00Z"
        );
        assert!(response.view_model.details_label.contains("1 warning"));
        for forbidden in ["Alice", ".minecraft", "api_token", "secret"] {
            assert!(!encoded.contains(forbidden), "{forbidden}");
        }
    }

    fn test_plan(
        composition_id: &str,
        tier: CompositionTier,
        mods: Vec<ManagedMod>,
        fallback_reason: &str,
        warnings: Vec<String>,
    ) -> CompositionPlan {
        CompositionPlan {
            composition_id: composition_id.to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier,
            mods,
            jvm_preset: String::new(),
            warnings,
            fallback_reason: fallback_reason.to_string(),
        }
    }

    fn test_mod(project_id: &str, name: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: project_id.to_string(),
            project_id: project_id.to_string(),
            slug: project_id.to_string(),
            name: name.to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }
}
