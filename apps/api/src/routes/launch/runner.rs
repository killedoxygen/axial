use crate::logging::append_trace;
use crate::state::launch_reports::{LaunchProofContext, LaunchProofResourceBudget};
use crate::state::{AppState, LaunchStatusEvent, StartupOutcome};
use croopor_config::{AppConfig, Instance};
use croopor_launcher::{
    GuardianInterventionKind, GuardianSummary, LaunchFailureClass, LaunchPreparationEvent,
    LaunchState, PreLaunchAction, PreLaunchDecision, RecoveryAction, StartupFailureDecision,
    StartupFailureObservation, build_healing_summary, decide_prepare_failure,
    decide_startup_failure, failure_class_name, guidance_for_failure, launch_state_name,
    prepare_launch_attempt_with_events, recovery_plan_for_startup_failure,
};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const PREWARM_MAX_FILES: usize = 8;
const PREWARM_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
const PREWARM_MAX_FILE_BYTES: u64 = 256 * 1024;
const PREWARM_REDUCED_MAX_FILES: usize = 2;
const PREWARM_REDUCED_MAX_TOTAL_BYTES: u64 = 512 * 1024;
const PREWARM_REDUCED_MAX_FILE_BYTES: u64 = 128 * 1024;
const PREWARM_BUFFER_BYTES: usize = 16 * 1024;
const LIVE_LAUNCH_FAILURE_MAX_CHARS: usize = 180;
const LIVE_LAUNCH_FAILURE_SAFE_FALLBACK: &str = "Launch failed before Minecraft could start. Detailed diagnostics were hidden because they may contain local paths or secrets.";

pub(super) struct LaunchSuccess {
    pub session_id: String,
    pub instance_id: String,
    pub pid: u32,
    pub launched_at: String,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
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
        state
            .sessions()
            .emit_log(
                &session_id,
                "system",
                format!("Preparing launch for {}.", instance.name),
            )
            .await;

        let (preparation_event_tx, mut preparation_event_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let preparation_event_sender = preparation_event_tx.clone();
        let preparation_status_state = state.clone();
        let preparation_status_session_id = session_id.clone();
        let preparation_status_guardian = guardian.clone();
        let preparation_status_task = tokio::spawn(async move {
            while let Some(event) = preparation_event_rx.recv().await {
                emit_status(
                    &preparation_status_state,
                    &preparation_status_session_id,
                    launch_state_for_preparation_event(event),
                    None,
                    None,
                    None,
                    Some(preparation_status_guardian.clone()),
                )
                .await;
            }
        });
        let prepared_result = prepare_launch_attempt_with_events(&intent, &attempt, move |event| {
            let _ = preparation_event_sender.send(event);
        })
        .await;
        drop(preparation_event_tx);
        let _ = preparation_status_task.await;

        let prepared = match prepared_result {
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
                        block_guardian_with_reason_and_guidance(
                            &mut guardian,
                            bounded_prepare_failure_reason(class, &message),
                            guidance,
                        );
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
                block_guardian_with_reason_and_guidance(
                    &mut guardian,
                    bounded_prepare_failure_reason(failure_class, &error.message),
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

        emit_status(
            &state,
            &session_id,
            LaunchState::Preparing,
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
        let prewarm =
            prewarm_launch_plan(&prepared.plan, proof_context.resource_budget.as_ref()).await;
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
                    max_memory_mb: intent.max_memory_mb,
                    min_memory_mb: intent.min_memory_mb,
                    healing: prepared.healing.clone(),
                    guardian: Some(guardian.clone()),
                });
            }
            StartupOutcome::Exited | StartupOutcome::Stalled => {
                let stalled = matches!(outcome, StartupOutcome::Stalled);
                if stalled {
                    let _ = state.sessions().kill(&session_id).await;
                }

                let observation = if stalled {
                    StartupFailureObservation::Stalled
                } else {
                    StartupFailureObservation::Exited {
                        failure_class: state
                            .sessions()
                            .observed_failure(&session_id)
                            .await
                            .unwrap_or(LaunchFailureClass::Unknown),
                    }
                };
                let startup_decision = decide_startup_failure(&intent.guardian, observation);
                let failure_class = startup_decision.class;
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
                        auth_mode: if intent.auth.user_type == "msa" {
                            "online"
                        } else {
                            "offline"
                        },
                        requested_java_path: &intent.requested_java,
                        requested_preset: &intent.requested_preset,
                        effective_java_path: Some(prepared.runtime.effective_path.as_str()),
                        effective_preset: Some(prepared.effective_preset.as_str()),
                        fallback_applied: attempt.fallback_applied.as_deref(),
                        retry_count: attempt.retry_count,
                        failure_class: Some(failure_class),
                    });
                apply_startup_failure_guardian_decision(&mut guardian, &startup_decision);
                return Err(fail_launch(
                    &state,
                    &session_id,
                    Some(&proof_context),
                    failure_class,
                    &startup_decision.message,
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

fn block_guardian_with_reason_and_guidance(
    guardian: &mut GuardianSummary,
    reason: Option<String>,
    guidance: Vec<String>,
) {
    let mut merged = guardian.guidance.clone();
    for detail in guidance {
        push_unique_detail(&mut merged, detail);
    }
    if let Some(reason) = reason {
        guardian.block_with_reason_and_guidance(reason, merged);
    } else {
        guardian.block_with_guidance(merged);
    }
}

fn apply_startup_failure_guardian_decision(
    guardian: &mut GuardianSummary,
    decision: &StartupFailureDecision,
) {
    let mut merged = guardian.guidance.clone();
    for detail in &decision.guidance {
        push_unique_detail(&mut merged, detail.clone());
    }
    guardian.block_with_message_reason_and_guidance(
        decision.message.clone(),
        decision.reason.clone(),
        merged,
    );
}

fn bounded_prepare_failure_reason(
    failure_class: LaunchFailureClass,
    message: &str,
) -> Option<String> {
    if failure_class == LaunchFailureClass::Unknown {
        return None;
    }
    bounded_guardian_detail(message)
}

fn bounded_guardian_detail(message: &str) -> Option<String> {
    let detail = message.trim().replace(
        "-XX:+UnlockExperimentalVMOptions",
        "the required experimental JVM unlock flag",
    );
    if detail.is_empty() || detail.len() > 240 || contains_guardian_unsafe_detail(&detail) {
        return None;
    }
    Some(detail)
}

fn contains_guardian_unsafe_detail(detail: impl AsRef<str>) -> bool {
    let detail = detail.as_ref();
    let lower = detail.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("account")
        || lower.contains("username")
        || detail.contains("-X")
        || detail.contains("-D")
        || detail.contains('/')
        || detail.contains('\\')
        || detail.contains('\n')
        || detail.contains('\r')
        || detail.contains("${")
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

async fn prewarm_launch_plan(
    plan: &croopor_launcher::VanillaLaunchPlan,
    resource_budget: Option<&LaunchProofResourceBudget>,
) -> LaunchPrewarmRunSummary {
    // Prewarm is bounded and best-effort. Resource pressure reduces or skips it.
    let selection = select_prewarm_budget(resource_budget);
    let candidate_paths = prewarm_candidate_paths(plan);
    let summary = match selection.budget {
        Some(budget) => prewarm_candidate_files(candidate_paths, budget).await,
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

async fn prewarm_candidate_files<I, P>(
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
        match prewarm_file_prefix(path.as_ref(), max_bytes).await {
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

async fn prewarm_file_prefix(path: &Path, max_bytes: u64) -> std::io::Result<u64> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut warmed = 0u64;
    let mut buffer = [0u8; PREWARM_BUFFER_BYTES];

    while warmed < max_bytes {
        let remaining = max_bytes.saturating_sub(warmed);
        let limit = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = file.read(&mut buffer[..limit]).await?;
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
    // Live failure details are sanitized before they enter status events or API responses.
    let public_message = sanitize_live_launch_failure_message(message);
    emit_terminal_failure(
        state,
        session_id,
        failure_class,
        &public_message,
        healing.clone(),
        guardian.clone(),
    )
    .await;
    persist_launch_proof_best_effort_with_context(state, session_id, None, "failed", proof_context)
        .await;
    LaunchRequestError {
        message: public_message,
        healing,
        guardian,
    }
}

pub(super) fn sanitize_live_launch_failure_message(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = single_line.trim();
    if trimmed.is_empty() || contains_live_launch_unsafe_failure_detail(message) {
        return LIVE_LAUNCH_FAILURE_SAFE_FALLBACK.to_string();
    }

    bound_live_launch_failure_message(trimmed)
}

fn contains_live_launch_unsafe_failure_detail(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("account")
        || lower.contains("provider")
        || lower.contains("username")
        || lower.contains("user")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("sessionid")
        || lower.contains("clientid")
        || lower.contains("java.exe")
        || detail.contains("-X")
        || detail.contains("-D")
        || detail.contains("--")
        || detail.contains('/')
        || detail.contains('\\')
        || detail.contains('\n')
        || detail.contains('\r')
        || detail.contains("${")
        || detail.contains('@')
        || is_token_like_live_launch_value(detail)
}

fn is_token_like_live_launch_value(value: &str) -> bool {
    value
        .split(|ch: char| ch.is_ascii_whitespace())
        .any(|part| {
            let parts: Vec<&str> = part.split('.').collect();
            parts.len() == 3
                && parts.iter().all(|segment| {
                    segment.len() >= 8
                        && segment
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                })
        })
}

fn bound_live_launch_failure_message(message: &str) -> String {
    let mut chars = message.chars();
    let bounded: String = chars.by_ref().take(LIVE_LAUNCH_FAILURE_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

fn launch_state_for_preparation_event(event: LaunchPreparationEvent) -> LaunchState {
    match event {
        LaunchPreparationEvent::Planning => LaunchState::Planning,
        LaunchPreparationEvent::EnsuringRuntime => LaunchState::EnsuringRuntime,
        LaunchPreparationEvent::DownloadingRuntime => LaunchState::DownloadingRuntime,
        LaunchPreparationEvent::Validating => LaunchState::Validating,
        LaunchPreparationEvent::Preparing => LaunchState::Preparing,
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
    let _ = state.update_config(next);
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
    use crate::state::{AppStateInit, InstallStore, LaunchEvent, SessionStore};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{GuardianMode, LaunchSessionRecord, SessionId};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;
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
    fn launch_guardian_block_preserves_reason_before_warning_guidance() {
        let warning = "Launch memory budget is tight.".to_string();
        let guidance = "Remove the Java override or switch Guardian Mode back to Managed.";
        let reason = "explicit Java override targets Java 8 but this version requires Java 17";
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);

        block_guardian_with_reason_and_guidance(
            &mut guardian,
            Some(format!(" {reason} ")),
            vec![guidance.to_string(), warning.clone()],
        );

        assert_eq!(
            guardian.details,
            vec![reason.to_string(), warning, guidance.to_string()]
        );
    }

    #[test]
    fn startup_stalled_blocks_with_guardian_authored_status_payload() {
        let warning = "Launch memory budget is tight.".to_string();
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.warn_with_guidance(vec![warning.clone()]);
        let context = croopor_launcher::LaunchGuardianContext {
            mode: GuardianMode::Managed,
            ..croopor_launcher::LaunchGuardianContext::default()
        };

        let decision = decide_startup_failure(&context, StartupFailureObservation::Stalled);
        apply_startup_failure_guardian_decision(&mut guardian, &decision);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(decision.class, LaunchFailureClass::StartupStalled);
        assert_eq!(
            guardian.decision,
            croopor_launcher::GuardianDecision::Blocked
        );
        assert_eq!(guardian.message.as_deref(), Some(decision.message.as_str()));
        assert_eq!(guardian.details.first(), Some(&decision.reason));
        assert!(guardian.details.iter().any(|detail| detail == &warning));
        assert!(
            guardian
                .details
                .iter()
                .any(|detail| detail == "Review the latest game log before retrying.")
        );
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(payload["message"], serde_json::json!(decision.message));
        assert_eq!(payload["details"][0], serde_json::json!(decision.reason));
    }

    #[test]
    fn startup_exited_blocks_with_observed_failure_guardian_summary() {
        let mut guardian = GuardianSummary::new(GuardianMode::Custom);
        let context = croopor_launcher::LaunchGuardianContext {
            mode: GuardianMode::Custom,
            raw_jvm_args_origin: Some(croopor_launcher::OverrideOrigin::Instance),
            ..croopor_launcher::LaunchGuardianContext::default()
        };

        let decision = decide_startup_failure(
            &context,
            StartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
            },
        );
        apply_startup_failure_guardian_decision(&mut guardian, &decision);
        let payload = serialize_guardian(Some(guardian.clone())).expect("guardian payload");

        assert_eq!(decision.class, LaunchFailureClass::JvmUnsupportedOption);
        assert_eq!(
            guardian.decision,
            croopor_launcher::GuardianDecision::Blocked
        );
        assert_eq!(
            guardian.message.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            guardian.details,
            vec![
                "Minecraft exited before startup completed with a detected JVM option compatibility failure.",
                "Remove the explicit JVM args or switch Guardian Mode back to Managed.",
            ]
        );
        assert_eq!(payload["decision"], serde_json::json!("blocked"));
        assert_eq!(payload["details"][0], serde_json::json!(decision.reason));
    }

    #[tokio::test]
    async fn fail_launch_sanitizes_public_error_and_terminal_failure_payloads() {
        let root = unique_test_dir("live-launch-failure");
        let state = test_app_state(&root);
        let session_id = "unsafe-live-failure";
        state.sessions().insert(test_record(session_id)).await;
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe");
        let unsafe_message = "prepare failed for /home/alice/.croopor/instances/secret java.exe --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer\nnext command fragment C:\\Users\\Alice\\AppData\\java.exe eyJheader123456789.abcdEFGH12345678.ijklMNOP12345678";

        let error = fail_launch(
            &state,
            session_id,
            None,
            LaunchFailureClass::Unknown,
            unsafe_message,
            None,
            None,
        )
        .await;

        assert_safe_live_launch_failure_text(&error.message);
        assert!(
            error
                .message
                .contains("Launch failed before Minecraft could start")
        );

        let log_event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("log event")
            .expect("log event result");
        let log_text = match log_event {
            LaunchEvent::Log(log) => log.text,
            other => panic!("expected log event, got {other:?}"),
        };
        assert_safe_live_launch_failure_text(&log_text);
        assert!(log_text.contains("Detailed diagnostics were hidden"));

        let status_event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("status event")
            .expect("status event result");
        let failure_detail = match status_event {
            LaunchEvent::Status(status) => {
                assert_eq!(status.state, "exited");
                status.failure_detail.expect("failure detail")
            }
            other => panic!("expected status event, got {other:?}"),
        };
        assert_safe_live_launch_failure_text(&failure_detail);
        assert!(failure_detail.contains("Detailed diagnostics were hidden"));

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal failure session record");
        assert_eq!(record.state, LaunchState::Exited);
        assert!(record.failure.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_launch_failure_sanitizer_keeps_safe_bounded_errors_useful() {
        let message = "launch plan did not produce a runnable command after preparation completed";

        let sanitized = sanitize_live_launch_failure_message(message);

        assert_eq!(sanitized, message);
        assert_safe_live_launch_failure_text(&sanitized);
    }

    #[test]
    fn preparation_events_map_to_existing_launch_states() {
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Planning),
            LaunchState::Planning
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::EnsuringRuntime),
            LaunchState::EnsuringRuntime
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::DownloadingRuntime),
            LaunchState::DownloadingRuntime
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Validating),
            LaunchState::Validating
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Preparing),
            LaunchState::Preparing
        );
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

    #[tokio::test]
    async fn launch_prewarm_reads_bounded_prefixes_and_skips_best_effort() {
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
        )
        .await;

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

    #[tokio::test]
    async fn launch_prewarm_caps_attempted_file_count() {
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
        )
        .await;

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

    fn test_app_state(root: &Path) -> AppState {
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

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn test_record(session_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            healing: None,
            guardian: None,
            stages: Vec::new(),
        }
    }

    fn assert_safe_live_launch_failure_text(text: &str) {
        assert!(text.chars().count() <= LIVE_LAUNCH_FAILURE_MAX_CHARS + 3);
        assert!(!text.contains('\n'));
        assert!(!text.contains('\r'));
        for fragment in [
            "/home/alice",
            "C:\\Users",
            "--accessToken",
            "-Xmx8192M",
            "-Dtoken",
            "raw-secret",
            "provider_payload",
            "provider-secret",
            "account_id",
            "account-secret",
            "username",
            "SecretPlayer",
            "java.exe",
            "eyJheader123456789",
        ] {
            assert!(
                !text.contains(fragment),
                "live launch failure leaked fragment {fragment:?}: {text}"
            );
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
