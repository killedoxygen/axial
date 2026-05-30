use crate::logging::append_trace;
use crate::state::launch_reports::{LaunchProofContext, LaunchProofResourceBudget};
use crate::state::{AppState, LaunchStatusEvent, StartupOutcome};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{
    GuardianInterventionKind, GuardianSummary, LaunchFailureClass, LaunchState, PreLaunchAction,
    PreLaunchDecision, RecoveryAction, build_healing_summary, decide_prepare_failure,
    failure_class_name, format_failure_class, guidance_for_failure, launch_state_name,
    prepare_launch_attempt, recovery_plan_for_startup_failure,
};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::process::Command;

const PREWARM_MAX_FILES: usize = 8;
const PREWARM_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
const PREWARM_MAX_FILE_BYTES: u64 = 256 * 1024;
const PREWARM_REDUCED_MAX_FILES: usize = 2;
const PREWARM_REDUCED_MAX_TOTAL_BYTES: u64 = 512 * 1024;
const PREWARM_REDUCED_MAX_FILE_BYTES: u64 = 128 * 1024;
const PREWARM_BUFFER_BYTES: usize = 16 * 1024;

pub(super) struct LaunchSuccess {
    pub session_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub launched_at: String,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub(super) struct LaunchRequestError {
    pub message: String,
    pub healing: Option<croopor_launcher::LaunchHealingSummary>,
    pub guardian: Option<GuardianSummary>,
}

pub(super) async fn launch_session(
    state: AppState,
    task: super::task::LaunchSessionTask,
) -> Result<LaunchSuccess, LaunchRequestError> {
    let super::task::LaunchSessionTask {
        mut instance,
        config,
        intent,
        mut guardian,
        launched_at,
        benchmark,
        resource_budget,
    } = task;
    let session_id = intent.session_id.clone();
    if let Some(benchmark_payload) = benchmark.as_ref().map(super::benchmark_status_payload) {
        state
            .sessions()
            .attach_benchmark(&session_id, benchmark_payload)
            .await;
    }
    let proof_context = LaunchProofContext::from_intent(&intent)
        .with_benchmark(benchmark)
        .with_resource_budget(resource_budget);
    let mut attempt = croopor_launcher::service::AttemptOverrides::default();

    loop {
        trace_launch_event(&session_id, "launch_session entered");
        emit_status(
            &state,
            &session_id,
            LaunchState::Validating,
            None,
            None,
            None,
            Some(guardian.clone()),
        )
        .await;
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!("Preparing launch for {}.", instance.name),
            )
            .await;

        let prepared = match prepare_launch_attempt(&intent, &attempt).await {
            Ok(prepared) => prepared,
            Err(error) => {
                trace_launch_event(&session_id, &format!("prepare failed: {}", error.message));
                let failure_class = error.failure_class.unwrap_or(LaunchFailureClass::Unknown);
                match decide_prepare_failure(
                    &intent.guardian,
                    failure_class,
                    &error.message,
                    &intent.requested_java,
                    &intent.extra_jvm_args,
                    attempt.runtime_intervention_applied,
                    attempt.raw_jvm_args_intervention_applied,
                ) {
                    PreLaunchDecision::Allow => {}
                    PreLaunchDecision::Intervene {
                        action,
                        kind,
                        description,
                    } => {
                        state
                            .sessions()
                            .emit_log(&session_id, "system", description.clone())
                            .await;
                        record_guardian_intervention(
                            &mut guardian,
                            kind,
                            description.clone(),
                            false,
                        );
                        match action {
                            PreLaunchAction::ForceManagedRuntime => {
                                attempt.record_runtime_intervention(description);
                            }
                            PreLaunchAction::StripRawJvmArgs => {
                                attempt.record_raw_jvm_args_intervention(description);
                            }
                        }
                        continue;
                    }
                    PreLaunchDecision::Block {
                        class,
                        message,
                        guidance,
                    } => {
                        block_guardian_with_guidance(&mut guardian, guidance);
                        return Err(fail_launch(
                            &state,
                            &session_id,
                            Some(&proof_context),
                            class,
                            &message,
                            error.healing,
                            Some(guardian.clone()),
                        )
                        .await);
                    }
                }
                block_guardian_with_guidance(
                    &mut guardian,
                    guidance_for_failure(failure_class, &intent.guardian),
                );
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    failure_class,
                    &error.message,
                    error.healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        };
        for intervention in &prepared.guardian_interventions {
            if let Some(detail) = intervention.detail.as_deref() {
                record_guardian_intervention(
                    &mut guardian,
                    intervention.kind,
                    detail,
                    intervention.silent.unwrap_or(false),
                );
            }
        }
        trace_launch_event(
            &session_id,
            &format!(
                "prepare finished total={}ms version={}ms runtime={}ms planning={}ms",
                prepared.metrics.total_ms,
                prepared.metrics.version_ms,
                prepared.metrics.runtime_ms,
                prepared.metrics.planning_ms,
            ),
        );

        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Launch prep finished in {} ms (version {} ms, runtime {} ms, plan {} ms).",
                    prepared.metrics.total_ms,
                    prepared.metrics.version_ms,
                    prepared.metrics.runtime_ms,
                    prepared.metrics.planning_ms,
                ),
            )
            .await;

        if prepared.runtime.effective_source == "managed" {
            emit_status(
                &state,
                &session_id,
                LaunchState::EnsuringRuntime,
                None,
                None,
                prepared.healing.clone(),
                Some(guardian.clone()),
            )
            .await;
        }

        emit_status(
            &state,
            &session_id,
            LaunchState::Planning,
            None,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!(
                    "Using Java {} via {}.",
                    prepared.runtime.effective_info.major, prepared.runtime.effective_source
                ),
            )
            .await;

        if prepared.plan.command.len() < 2 {
            return Err(fail_launch(
                &state,
                &session_id,
                Some(&proof_context),
                LaunchFailureClass::Unknown,
                "launch plan did not produce a runnable command",
                prepared.healing.clone(),
                Some(guardian.clone()),
            )
            .await);
        }

        emit_status(
            &state,
            &session_id,
            LaunchState::Prewarming,
            None,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;
        let prewarm = prewarm_launch_plan(&prepared.plan, proof_context.resource_budget.as_ref());
        let prewarm_summary = format_prewarm_run_summary(&prewarm);
        trace_launch_event(&session_id, &prewarm_summary);
        state
            .sessions()
            .emit_log(&session_id, "system", prewarm_summary)
            .await;

        let mut command = Command::new(&prepared.plan.command[0]);
        command.args(&prepared.plan.command[1..]);
        command.current_dir(&prepared.plan.game_dir);

        let record = crate::state::LaunchSessionRecord {
            session_id: croopor_launcher::SessionId(session_id.clone()),
            instance_id: intent.instance_id.clone(),
            version_id: intent.version_id.clone(),
            launched_at: Some(launched_at.clone()),
            benchmark: None,
            state: LaunchState::Starting,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: prepared.plan.command.clone(),
            java_path: Some(prepared.runtime.effective_path.clone()),
            natives_dir: prepared
                .plan
                .natives_dir
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            failure: None,
            healing: prepared
                .healing
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok()),
            guardian: serialize_guardian(Some(guardian.clone())),
            stages: Vec::new(),
        };

        let launched = match state.sessions().start_process(record, command).await {
            Ok(record) => record,
            Err(error) => {
                trace_launch_event(&session_id, &format!("spawn failed: {error}"));
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    LaunchFailureClass::Unknown,
                    &format!("failed to start launch process: {error}"),
                    prepared.healing.clone(),
                    Some(guardian.clone()),
                )
                .await);
            }
        };
        trace_launch_event(&session_id, &format!("spawned pid={:?}", launched.pid));

        emit_status(
            &state,
            &session_id,
            LaunchState::Monitoring,
            launched.pid,
            None,
            prepared.healing.clone(),
            Some(guardian.clone()),
        )
        .await;

        let outcome = state
            .sessions()
            .wait_for_startup(&session_id, std::time::Duration::from_secs(5))
            .await;

        match outcome {
            StartupOutcome::Stable | StartupOutcome::TimedOut => {
                emit_status(
                    &state,
                    &session_id,
                    LaunchState::Running,
                    launched.pid,
                    None,
                    prepared.healing.clone(),
                    Some(guardian.clone()),
                )
                .await;
                persist_launch_proof_best_effort_with_context(
                    &state,
                    &session_id,
                    Some(launched_at.as_str()),
                    "running",
                    Some(&proof_context),
                )
                .await;
                persist_launch_metadata(
                    &state,
                    &mut instance,
                    &config,
                    &intent.username,
                    intent.max_memory_mb,
                    intent.min_memory_mb,
                    &launched_at,
                );
                return Ok(LaunchSuccess {
                    session_id: session_id.clone(),
                    instance_id: intent.instance_id.clone(),
                    pid: launched.pid.unwrap_or_default(),
                    launched_at: launched_at.clone(),
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id).await;
                }

                let failure_class = if stalled {
                    LaunchFailureClass::StartupStalled
                } else {
                    state
                        .sessions()
                        .observed_failure(&session_id)
                        .await
                        .map(|failure| failure.class)
                        .unwrap_or(LaunchFailureClass::Unknown)
                };
                if !attempt.startup_recovery_applied
                    && let Some(recovery) = recovery_plan_for_startup_failure(
                        failure_class,
                        &intent.version_id,
                        &prepared.runtime.effective_info,
                        &intent.requested_java,
                        &intent.guardian,
                        attempt.disable_custom_gc,
                        &prepared.effective_preset,
                    )
                {
                    state
                        .sessions()
                        .emit_log(&session_id, "system", recovery.description.clone())
                        .await;
                    attempt.record_startup_recovery(recovery.description.clone());
                    match recovery.action {
                        RecoveryAction::DowngradePreset(next_preset) => {
                            record_guardian_intervention(
                                &mut guardian,
                                GuardianInterventionKind::DowngradePreset,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.preset_override = Some(next_preset);
                            attempt.disable_custom_gc = false;
                        }
                        RecoveryAction::DisableCustomGc => {
                            record_guardian_intervention(
                                &mut guardian,
                                GuardianInterventionKind::DisableCustomGc,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.preset_override = None;
                            attempt.disable_custom_gc = true;
                        }
                        RecoveryAction::SwitchManagedRuntime => {
                            record_guardian_intervention(
                                &mut guardian,
                                GuardianInterventionKind::SwitchManagedRuntime,
                                recovery.description.clone(),
                                false,
                            );
                            attempt.force_managed_runtime = true;
                            attempt.preset_override = None;
                            attempt.disable_custom_gc = false;
                        }
                    }
                    continue;
                }

                let healing =
                    build_healing_summary(croopor_launcher::service::HealingSummaryInput {
                        requested_java_path: &intent.requested_java,
                        requested_preset: &intent.requested_preset,
                        effective_java_path: Some(prepared.runtime.effective_path.as_str()),
                        effective_preset: Some(prepared.effective_preset.as_str()),
                        fallback_applied: attempt.fallback_applied.as_deref(),
                        retry_count: attempt.retry_count,
                        failure_class: Some(failure_class),
                    });
                block_guardian_with_guidance(
                    &mut guardian,
                    guidance_for_failure(failure_class, &intent.guardian),
                );
                let message = if failure_class == LaunchFailureClass::StartupStalled {
                    "launch stopped before startup: no startup activity observed".to_string()
                } else {
                    format!(
                        "launch failed during startup: {}",
                        format_failure_class(failure_class)
                    )
                };
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    failure_class,
                    &message,
                    healing,
                    Some(guardian.clone()),
                )
                .await);
            }
        }
    }
}

pub(super) fn trace_launch_event(session_id: &str, message: &str) {
    append_trace("launch", session_id, message);
}

fn record_guardian_intervention(
    guardian: &mut GuardianSummary,
    kind: GuardianInterventionKind,
    detail: impl Into<String>,
    silent: bool,
) {
    let existing_guidance = guardian.guidance.clone();
    guardian.record_intervention(kind, detail, silent);
    append_guardian_guidance_details(guardian, &existing_guidance);
}

fn block_guardian_with_guidance(guardian: &mut GuardianSummary, guidance: Vec<String>) {
    let mut merged = guardian.guidance.clone();
    for detail in guidance {
        push_unique_detail(&mut merged, detail);
    }
    guardian.block_with_guidance(merged);
}

fn append_guardian_guidance_details(guardian: &mut GuardianSummary, guidance: &[String]) {
    for detail in guidance {
        push_unique_detail(&mut guardian.details, detail.clone());
    }
}

fn push_unique_detail(details: &mut Vec<String>, detail: String) {
    let detail = detail.trim();
    if detail.is_empty() || details.iter().any(|existing| existing == detail) {
        return;
    }
    details.push(detail.to_string());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchPrewarmBudget {
    max_files: usize,
    max_total_bytes: u64,
    max_file_bytes: u64,
}

impl Default for LaunchPrewarmBudget {
    fn default() -> Self {
        Self {
            max_files: PREWARM_MAX_FILES,
            max_total_bytes: PREWARM_MAX_TOTAL_BYTES,
            max_file_bytes: PREWARM_MAX_FILE_BYTES,
        }
    }
}

impl LaunchPrewarmBudget {
    fn reduced() -> Self {
        Self {
            max_files: PREWARM_REDUCED_MAX_FILES,
            max_total_bytes: PREWARM_REDUCED_MAX_TOTAL_BYTES,
            max_file_bytes: PREWARM_REDUCED_MAX_FILE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchPrewarmBudgetClass {
    Default,
    Reduced,
    Skipped,
}

impl LaunchPrewarmBudgetClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Reduced => "reduced",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchPrewarmSelection {
    class: LaunchPrewarmBudgetClass,
    budget: Option<LaunchPrewarmBudget>,
    reason: Option<&'static str>,
}

impl LaunchPrewarmSelection {
    fn default_budget() -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Default,
            budget: Some(LaunchPrewarmBudget::default()),
            reason: None,
        }
    }

    fn reduced(reason: &'static str) -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Reduced,
            budget: Some(LaunchPrewarmBudget::reduced()),
            reason: Some(reason),
        }
    }

    fn skipped(reason: &'static str) -> Self {
        Self {
            class: LaunchPrewarmBudgetClass::Skipped,
            budget: None,
            reason: Some(reason),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LaunchPrewarmSummary {
    warmed_files: usize,
    warmed_bytes: u64,
    skipped_files: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchPrewarmRunSummary {
    selection: LaunchPrewarmSelection,
    warmed_files: usize,
    warmed_bytes: u64,
    skipped_files: usize,
}

fn prewarm_launch_plan(
    plan: &croopor_launcher::VanillaLaunchPlan,
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> LaunchPrewarmRunSummary {
    let selection = select_prewarm_budget(resource_budget);
    let candidate_paths = prewarm_candidate_paths(plan);
    let summary = match selection.budget {
        Some(budget) => prewarm_candidate_files(candidate_paths, budget),
        None => LaunchPrewarmSummary {
            skipped_files: candidate_paths.len(),
            ..LaunchPrewarmSummary::default()
        },
    };

    LaunchPrewarmRunSummary {
        selection,
        warmed_files: summary.warmed_files,
        warmed_bytes: summary.warmed_bytes,
        skipped_files: summary.skipped_files,
    }
}

fn select_prewarm_budget(
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> LaunchPrewarmSelection {
    let Some(resource_budget) = resource_budget else {
        return LaunchPrewarmSelection::default_budget();
    };

    if resource_budget.cpu_pressure && resource_budget.install_pressure {
        return LaunchPrewarmSelection::skipped("cpu_and_install_pressure");
    }
    if resource_budget.disk_pressure
        && resource_budget
            .launch_disk_available_mb
            .is_some_and(|available_mb| available_mb < resource_budget.launch_disk_headroom_mb)
    {
        return LaunchPrewarmSelection::skipped("disk_headroom_pressure");
    }
    if has_prewarm_pressure(resource_budget) {
        return LaunchPrewarmSelection::reduced("resource_pressure");
    }

    LaunchPrewarmSelection::default_budget()
}

fn has_prewarm_pressure(resource_budget: &LaunchProofResourceBudget) -> bool {
    resource_budget.cpu_pressure
        || resource_budget.install_pressure
        || resource_budget.disk_pressure
        || resource_budget.active_session_count > 0
}

fn format_prewarm_run_summary(prewarm: &LaunchPrewarmRunSummary) -> String {
    let reason = prewarm
        .selection
        .reason
        .map(|reason| format!(" reason={reason}"))
        .unwrap_or_default();
    format!(
        "Prewarmed launch data: mode={} warmed_files={} warmed_bytes={} skipped={}{}.",
        prewarm.selection.class.as_str(),
        prewarm.warmed_files,
        prewarm.warmed_bytes,
        prewarm.skipped_files,
        reason
    )
}

fn prewarm_candidate_paths(plan: &croopor_launcher::VanillaLaunchPlan) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    if let Some(client_jar_path) = plan.client_jar_path.as_ref() {
        push_unique_prewarm_path(&mut paths, &mut seen, client_jar_path);
    }
    for library in &plan.libraries {
        if !library.is_native && is_jar_path(&library.abs_path) {
            push_unique_prewarm_path(&mut paths, &mut seen, &library.abs_path);
        }
    }

    paths
}

fn push_unique_prewarm_path(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: &Path) {
    let path = path.to_path_buf();
    if seen.insert(path.clone()) {
        paths.push(path);
    }
}

fn is_jar_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
}

fn prewarm_candidate_files<I, P>(
    candidate_paths: I,
    budget: LaunchPrewarmBudget,
) -> LaunchPrewarmSummary
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut summary = LaunchPrewarmSummary::default();
    let mut attempted_files = 0usize;

    for path in candidate_paths {
        if attempted_files >= budget.max_files || summary.warmed_bytes >= budget.max_total_bytes {
            summary.skipped_files += 1;
            continue;
        }

        let remaining_total = budget.max_total_bytes.saturating_sub(summary.warmed_bytes);
        let max_bytes = budget.max_file_bytes.min(remaining_total);
        if max_bytes == 0 {
            summary.skipped_files += 1;
            continue;
        }

        attempted_files += 1;
        match prewarm_file_prefix(path.as_ref(), max_bytes) {
            Ok(bytes) => {
                summary.warmed_files += 1;
                summary.warmed_bytes = summary.warmed_bytes.saturating_add(bytes);
            }
            Err(_) => {
                summary.skipped_files += 1;
            }
        }
    }

    summary
}

fn prewarm_file_prefix(path: &Path, max_bytes: u64) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut warmed = 0u64;
    let mut buffer = [0u8; PREWARM_BUFFER_BYTES];

    while warmed < max_bytes {
        let remaining = max_bytes.saturating_sub(warmed);
        let limit = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = file.read(&mut buffer[..limit])?;
        if read == 0 {
            break;
        }
        warmed = warmed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }

    Ok(warmed)
}

async fn fail_launch(
    state: &AppState,
    session_id: &str,
    proof_context: Option<&LaunchProofContext>,
    failure_class: LaunchFailureClass,
    message: &str,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) -> LaunchRequestError {
    emit_terminal_failure(
        state,
        session_id,
        failure_class,
        message,
        healing.clone(),
        guardian.clone(),
    )
    .await;
    persist_launch_proof_best_effort_with_context(state, session_id, None, "failed", proof_context)
        .await;
    state.sessions().remove(session_id).await;
    LaunchRequestError {
        message: message.to_string(),
        healing,
        guardian,
    }
}

async fn emit_status(
    state: &AppState,
    session_id: &str,
    launch_state: LaunchState,
    pid: Option<u32>,
    failure_class: Option<LaunchFailureClass>,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) {
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: launch_state_name(launch_state).to_string(),
                benchmark: None,
                pid,
                exit_code: None,
                failure_class: failure_class.map(failure_class_name).map(str::to_string),
                failure_detail: None,
                healing: serialize_healing(healing),
                guardian: serialize_guardian(guardian),
                stages: Vec::new(),
            },
        )
        .await;
}

async fn emit_terminal_failure(
    state: &AppState,
    session_id: &str,
    failure_class: LaunchFailureClass,
    message: &str,
    healing: Option<croopor_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) {
    state
        .sessions()
        .emit_log(session_id, "system", message.to_string())
        .await;
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: "exited".to_string(),
                benchmark: None,
                pid: None,
                exit_code: Some(-1),
                failure_class: Some(failure_class_name(failure_class).to_string()),
                failure_detail: Some(message.to_string()),
                healing: serialize_healing(healing),
                guardian: serialize_guardian(guardian),
                stages: Vec::new(),
            },
        )
        .await;
}

fn persist_launch_metadata(
    state: &AppState,
    instance: &mut Instance,
    config: &AppConfig,
    username: &str,
    max_memory_mb: i32,
    min_memory_mb: i32,
    launched_at: &str,
) {
    instance.last_played_at = launched_at.to_string();
    let _ = state.instances().update(instance.clone());
    let _ = state.instances().set_last_instance_id(instance.id.clone());

    let mut next = config.clone();
    next.username = username.to_string();
    if max_memory_mb > 0 {
        next.max_memory_mb = max_memory_mb;
    }
    if min_memory_mb > 0 {
        next.min_memory_mb = min_memory_mb;
    }
    let _ = state.config().update(next);
}

pub(super) async fn persist_launch_proof_best_effort(
    state: &AppState,
    session_id: &str,
    launched_at: Option<&str>,
    outcome: &str,
) {
    persist_launch_proof_best_effort_with_context(state, session_id, launched_at, outcome, None)
        .await;
}

async fn persist_launch_proof_best_effort_with_context(
    state: &AppState,
    session_id: &str,
    launched_at: Option<&str>,
    outcome: &str,
    proof_context: Option<&LaunchProofContext>,
) {
    let Some(record) = state.sessions().get(session_id).await else {
        trace_launch_event(session_id, "launch proof skipped: session record missing");
        return;
    };
    match crate::state::launch_reports::persist_record_with_context(
        state.config().paths(),
        &record,
        launched_at,
        outcome,
        proof_context,
    ) {
        Ok(_) => {
            trace_launch_event(session_id, "launch proof persisted");
            if let Err(error) = crate::state::benchmark_suites::update_run_state_for_session(
                state.config().paths(),
                session_id,
                outcome,
            ) {
                trace_launch_event(
                    session_id,
                    &format!("benchmark suite manifest state update failed: {error}"),
                );
            }
        }
        Err(error) => trace_launch_event(
            session_id,
            &format!("launch proof persistence failed: {error}"),
        ),
    }
}

fn serialize_healing(healing: Option<croopor_launcher::LaunchHealingSummary>) -> Option<Value> {
    healing.and_then(|value| serde_json::to_value(value).ok())
}

fn serialize_guardian(guardian: Option<GuardianSummary>) -> Option<Value> {
    guardian.and_then(|value| serde_json::to_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use croopor_launcher::GuardianMode;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launch_guardian_intervention_preserves_existing_warning_guidance() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);

        record_guardian_intervention(
            &mut guardian,
            GuardianInterventionKind::SwitchManagedRuntime,
            "Guardian switched to managed Java before launch.",
            false,
        );

        assert!(guardian.guidance.iter().any(|detail| detail == &warning));
        assert!(guardian.details.iter().any(|detail| detail == &warning));
    }

    #[test]
    fn launch_prewarm_selects_default_budget_without_pressure() {
        let budget = test_resource_budget();
        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Default);
        assert_eq!(selection.budget, Some(LaunchPrewarmBudget::default()));
        assert_eq!(selection.reason, None);
    }

    #[test]
    fn launch_prewarm_selects_reduced_budget_under_pressure() {
        let mut budget = test_resource_budget();
        budget.active_session_count = 1;

        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Reduced);
        assert_eq!(selection.budget, Some(LaunchPrewarmBudget::reduced()));
        assert_eq!(selection.reason, Some("resource_pressure"));
        let selected_budget = selection.budget.expect("reduced budget");
        assert!(selected_budget.max_files < PREWARM_MAX_FILES);
        assert!(selected_budget.max_total_bytes < PREWARM_MAX_TOTAL_BYTES);
        assert!(selected_budget.max_file_bytes < PREWARM_MAX_FILE_BYTES);
    }

    #[test]
    fn launch_prewarm_selects_skip_for_severe_pressure() {
        let mut budget = test_resource_budget();
        budget.cpu_pressure = true;
        budget.install_pressure = true;

        let selection = select_prewarm_budget(Some(&budget));

        assert_eq!(selection.class, LaunchPrewarmBudgetClass::Skipped);
        assert_eq!(selection.budget, None);
        assert_eq!(selection.reason, Some("cpu_and_install_pressure"));
    }

    #[test]
    fn launch_prewarm_reads_bounded_prefixes_and_skips_best_effort() {
        let dir = unique_test_dir("launch-prewarm");
        fs::create_dir_all(&dir).expect("create test dir");
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let third = dir.join("third.jar");
        let missing = dir.join("missing.jar");
        fs::write(&first, [1u8; 10]).expect("write first");
        fs::write(&second, [2u8; 10]).expect("write second");
        fs::write(&third, [3u8; 10]).expect("write third");

        let summary = prewarm_candidate_files(
            [&first, &second, &missing, &third],
            LaunchPrewarmBudget {
                max_files: 8,
                max_total_bytes: 12,
                max_file_bytes: 8,
            },
        );

        assert_eq!(
            summary,
            LaunchPrewarmSummary {
                warmed_files: 2,
                warmed_bytes: 12,
                skipped_files: 2,
            }
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn launch_prewarm_caps_attempted_file_count() {
        let dir = unique_test_dir("launch-prewarm-file-cap");
        fs::create_dir_all(&dir).expect("create test dir");
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let third = dir.join("third.jar");
        fs::write(&first, [1u8; 10]).expect("write first");
        fs::write(&second, [2u8; 10]).expect("write second");
        fs::write(&third, [3u8; 10]).expect("write third");

        let summary = prewarm_candidate_files(
            [&first, &second, &third],
            LaunchPrewarmBudget {
                max_files: 1,
                max_total_bytes: 1024,
                max_file_bytes: 8,
            },
        );

        assert_eq!(
            summary,
            LaunchPrewarmSummary {
                warmed_files: 1,
                warmed_bytes: 8,
                skipped_files: 2,
            }
        );

        let _ = fs::remove_dir_all(dir);
    }

    fn test_resource_budget() -> LaunchProofResourceBudget {
        LaunchProofResourceBudget {
            host_total_memory_mb: Some(16 * 1024),
            host_available_memory_mb: Some(12 * 1024),
            host_used_memory_mb: Some(4 * 1024),
            host_cpu_threads: Some(8),
            host_cpu_load_1m_x100: Some(100),
            host_cpu_load_5m_x100: Some(100),
            host_cpu_load_15m_x100: Some(100),
            launcher_process_memory_mb: Some(256),
            active_session_count: 0,
            active_install_count: 0,
            active_memory_allocation_mb: 0,
            requested_memory_mb: Some(4096),
            estimated_remaining_memory_mb: Some(12 * 1024),
            memory_headroom_mb: 2048,
            memory_pressure: false,
            cpu_pressure: false,
            install_pressure: false,
            launch_disk_available_mb: Some(16 * 1024),
            launch_disk_headroom_mb: 2048,
            disk_pressure: false,
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
