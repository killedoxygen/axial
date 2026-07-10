use super::LaunchApplicationError;
use crate::application::performance::{
    self, BenchmarkMatrix, BenchmarkSuiteRunSpec, benchmark_suite_manifest_run_inputs,
    benchmark_suite_run_descriptor, benchmark_suite_run_id,
};
use crate::state::benchmark_suite_drivers::{
    BenchmarkSuiteDriverStartError, BenchmarkSuiteDriverStoreError,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{AppState, LaunchStatusEvent};
use axial_launcher::{LaunchStageEvidence, LaunchState};
use axum::Json;
use axum::http::StatusCode;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::json;
use std::io;
use std::time::Duration;

pub(crate) const DEFAULT_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 30_000;
pub(crate) const MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 5_000;
pub(crate) const MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 3_600_000;
pub(crate) const MAX_BENCHMARK_SUITE_DRIVER_LIST: usize = 25;

pub(crate) const BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE: &str =
    "Could not load benchmark suite data. Check app data permissions and try again.";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchmarkLaunchRequest {
    #[serde(default)]
    pub(crate) instance_id: Option<String>,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub run_type: Option<String>,
    #[serde(default)]
    pub benchmark_mode: Option<String>,
    #[serde(default)]
    pub suite_mode: Option<String>,
    #[serde(default)]
    pub suite_id: Option<String>,
    #[serde(default)]
    pub run_index: Option<i64>,
    #[serde(default)]
    pub interval_ms: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct BenchmarkLaunchInput {
    pub(crate) launch: super::LaunchRequest,
    pub(crate) profile: Option<String>,
    pub(crate) run_type: Option<String>,
    pub(crate) benchmark_mode: Option<String>,
}

#[derive(Debug)]
pub(crate) struct BenchmarkSuiteLaunchInput {
    pub(crate) launch: super::LaunchRequest,
    pub(crate) suite_id: String,
    pub(crate) mode: String,
    pub(crate) run_index: usize,
    pub(crate) plan: Vec<BenchmarkSuiteRunSpec>,
}

#[derive(Debug)]
pub(crate) struct BenchmarkSuitePlanInput {
    pub(crate) launch: super::LaunchRequest,
    pub(crate) suite_id: String,
    pub(crate) mode: String,
    pub(crate) plan: Vec<BenchmarkSuiteRunSpec>,
    pub(crate) manifest: Option<crate::state::benchmark_suites::BenchmarkSuiteManifest>,
}

#[derive(Debug)]
pub(crate) enum BenchmarkSuiteDriverDecision {
    Active {
        suite: serde_json::Value,
        active_session_id: String,
    },
    Complete {
        suite: serde_json::Value,
    },
    Launch(BenchmarkSuiteLaunchInput),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BenchmarkSuiteDriverResumeSummary {
    pub pending: usize,
    pub resumed: usize,
    pub failed: usize,
}

pub(crate) fn spawn_restart_interrupted_benchmark_suite_drivers(state: &AppState) -> bool {
    let state = state.clone();
    tokio::spawn(async move {
        match resume_restart_interrupted_benchmark_suite_drivers(state).await {
            Ok(summary) if summary.pending > 0 => tracing::info!(
                pending = summary.pending,
                resumed = summary.resumed,
                failed = summary.failed,
                "benchmark suite drivers resumed after restart"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                error_class = error.class(),
                "benchmark suite driver restart reconciliation failed"
            ),
        }
    });
    true
}

pub(crate) async fn resume_restart_interrupted_benchmark_suite_drivers(
    state: AppState,
) -> Result<BenchmarkSuiteDriverResumeSummary, BenchmarkSuiteDriverStoreError> {
    // Application failures are recorded per driver; persistence failures abort reconciliation.
    let pending = state
        .benchmark_suite_drivers()
        .take_restart_interrupted_resumable_drivers()
        .await?;
    let mut summary = BenchmarkSuiteDriverResumeSummary {
        pending: pending.len(),
        ..BenchmarkSuiteDriverResumeSummary::default()
    };

    for status in pending {
        let prepared = match prepare_benchmark_suite_driver_resume(&state, &status.id).await {
            Ok(prepared) => prepared,
            Err(error) => {
                summary.failed += 1;
                state
                    .benchmark_suite_drivers()
                    .record_restart_resume_failed(
                        &status.id,
                        &benchmark_suite_api_error_message(&error),
                    )
                    .await?;
                continue;
            }
        };
        let started = match state
            .benchmark_suite_drivers()
            .start(
                prepared.suite_id,
                prepared.mode,
                prepared.interval_ms,
                prepared.summary,
            )
            .await
        {
            Ok(started) => started,
            Err(error) => {
                summary.failed += 1;
                let error = benchmark_suite_driver_start_error_response(error);
                state
                    .benchmark_suite_drivers()
                    .record_restart_resume_failed(
                        &status.id,
                        &benchmark_suite_api_error_message(&error),
                    )
                    .await?;
                continue;
            }
        };
        state
            .benchmark_suite_drivers()
            .record_restart_resume_started(&status.id)
            .await?;
        spawn_benchmark_suite_driver_loop(
            state.clone(),
            started.status.id,
            prepared.request,
            prepared.interval_ms,
            started.effect_owner,
        );
        summary.resumed += 1;
    }

    Ok(summary)
}

impl BenchmarkLaunchRequest {
    pub(crate) fn into_launch_input(
        self,
    ) -> Result<BenchmarkLaunchInput, (StatusCode, Json<serde_json::Value>)> {
        if self
            .suite_mode
            .as_deref()
            .and_then(trimmed_string)
            .is_some()
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": "suite_mode is only supported for benchmark suite requests" }),
                ),
            ));
        }
        let launch = self.launch_request()?;
        let benchmark_mode = self.benchmark_mode.as_deref().and_then(trimmed_string);
        Ok(BenchmarkLaunchInput {
            launch,
            profile: self.profile,
            run_type: self.run_type,
            benchmark_mode,
        })
    }

    #[cfg(test)]
    pub(crate) fn into_suite_launch_input(
        self,
    ) -> Result<BenchmarkSuiteLaunchInput, (StatusCode, Json<serde_json::Value>)> {
        self.into_suite_launch_input_with_manifest(None)
    }

    pub(crate) fn into_suite_launch_input_with_manifest(
        self,
        paths: Option<&axial_config::AppPaths>,
    ) -> Result<BenchmarkSuiteLaunchInput, (StatusCode, Json<serde_json::Value>)> {
        let requested_run_index = self.run_index;
        let manifest_paths = if requested_run_index.is_none() {
            paths
        } else {
            None
        };
        let input = self.into_suite_plan_input_with_manifest(manifest_paths)?;
        let run_index = match requested_run_index {
            Some(run_index) => validate_benchmark_suite_run_index(run_index, input.plan.len())?,
            None => match paths {
                Some(_) => crate::state::benchmark_suites::next_pending_run_index(
                    input.manifest.as_ref(),
                    input.plan.len(),
                )
                .ok_or_else(benchmark_suite_complete_error)?,
                None => validate_benchmark_suite_run_index(0, input.plan.len())?,
            },
        };

        Ok(BenchmarkSuiteLaunchInput {
            launch: input.launch,
            suite_id: input.suite_id,
            mode: input.mode,
            run_index,
            plan: input.plan,
        })
    }

    pub(crate) fn into_suite_plan_input_with_manifest(
        self,
        paths: Option<&axial_config::AppPaths>,
    ) -> Result<BenchmarkSuitePlanInput, (StatusCode, Json<serde_json::Value>)> {
        if self
            .benchmark_mode
            .as_deref()
            .and_then(trimmed_string)
            .is_some()
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": "benchmark_mode is only supported for benchmark launch requests" }),
                ),
            ));
        }
        let launch = self.launch_request()?;
        let mode = benchmark_suite_mode_or_default(self.suite_mode.as_deref())?;
        let suite_id = self
            .suite_id
            .as_deref()
            .and_then(crate::state::benchmark_suites::normalize_suite_id)
            .unwrap_or_else(|| {
                crate::state::benchmark_suites::derive_suite_id(&launch.instance_id, &mode)
            });
        let plan =
            performance::benchmark_suite_plan(&mode).ok_or_else(unsupported_suite_mode_error)?;
        let manifest = match paths {
            Some(paths) => crate::state::benchmark_suites::load(paths, &suite_id)
                .map_err(benchmark_suite_storage_error_response)?,
            None => None,
        };

        Ok(BenchmarkSuitePlanInput {
            launch,
            suite_id,
            mode,
            plan,
            manifest,
        })
    }

    pub(crate) fn launch_request(
        &self,
    ) -> Result<super::LaunchRequest, (StatusCode, Json<serde_json::Value>)> {
        let instance_id = self
            .instance_id
            .as_deref()
            .and_then(trimmed_string)
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "instance_id is required" })),
                )
            })?;

        Ok(super::LaunchRequest {
            instance_id,
            username: self.username.clone(),
            max_memory_mb: self.max_memory_mb,
            min_memory_mb: self.min_memory_mb,
            client_started_at_ms: self.client_started_at_ms,
        })
    }
}

pub(crate) async fn launch_benchmark(
    state: AppState,
    payload: BenchmarkLaunchRequest,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let input = payload.into_launch_input()?;
    let mut prepared = super::prepare_launch_session(&state, input.launch).await?;
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(prepared.task.intent.session_id.as_str()),
        input.profile.as_deref(),
        input.run_type.as_deref(),
        input.benchmark_mode.as_deref(),
    );
    let benchmark_response = super::launch_benchmark_status_payload(&benchmark);
    prepared.task.benchmark = Some(benchmark.clone());
    let launched = super::launch_session(state.clone(), prepared.task)
        .await
        .map_err(super::launch_request_error_response)?;

    let mut response = super::launch_success_response_payload(&launched);
    response["benchmark"] = benchmark_response;
    Ok(response)
}

pub(crate) async fn launch_benchmark_suite(
    state: AppState,
    payload: BenchmarkLaunchRequest,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let auto_next_run = payload.run_index.is_none();
    if auto_next_run {
        let launch = payload.launch_request()?;
        let mode = benchmark_suite_mode_or_default(payload.suite_mode.as_deref())?;
        let _ =
            performance::benchmark_suite_plan(&mode).ok_or_else(unsupported_suite_mode_error)?;
        let suite_id = payload
            .suite_id
            .as_deref()
            .and_then(crate::state::benchmark_suites::normalize_suite_id)
            .unwrap_or_else(|| {
                crate::state::benchmark_suites::derive_suite_id(&launch.instance_id, &mode)
            });
        let manifest = crate::state::benchmark_suites::load(state.config().paths(), &suite_id)
            .map_err(benchmark_suite_storage_error_response)?;
        ensure_no_active_benchmark_suite_auto_run(
            state.sessions().as_ref(),
            manifest.as_ref(),
            auto_next_run,
        )
        .await?;
    }

    let input = payload.into_suite_launch_input_with_manifest(Some(state.config().paths()))?;
    launch_benchmark_suite_run(state, input).await
}

pub(crate) async fn tick_benchmark_suite(
    state: AppState,
    payload: BenchmarkLaunchRequest,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let input = payload.into_suite_plan_input_with_manifest(Some(state.config().paths()))?;
    match benchmark_suite_driver_decision(state.sessions().as_ref(), input).await? {
        BenchmarkSuiteDriverDecision::Active {
            suite,
            active_session_id,
        } => Ok(json!({
            "status": "active",
            "driver": { "state": "active" },
            "suite": suite,
            "active_session_id": active_session_id,
        })),
        BenchmarkSuiteDriverDecision::Complete { suite } => Ok(json!({
            "status": "complete",
            "driver": { "state": "complete" },
            "suite": suite,
        })),
        BenchmarkSuiteDriverDecision::Launch(input) => {
            let mut payload = launch_benchmark_suite_run(state, input).await?;
            payload["driver"] = json!({ "state": "launched_next" });
            Ok(payload)
        }
    }
}

pub(crate) async fn start_benchmark_suite_driver(
    state: AppState,
    payload: BenchmarkLaunchRequest,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let interval_ms = clamp_benchmark_suite_driver_interval_ms(payload.interval_ms);
    let input = payload
        .clone()
        .into_suite_plan_input_with_manifest(Some(state.config().paths()))?;
    let summary = benchmark_suite_driver_suite_summary(&input);
    if summary.pending_run_index.is_none() {
        return Err(benchmark_suite_complete_error());
    }
    let mut driver_payload = payload.clone();
    driver_payload.suite_id = Some(input.suite_id.clone());
    driver_payload.suite_mode = Some(input.mode.clone());
    driver_payload.benchmark_mode = None;
    driver_payload.run_index = None;

    start_owned_benchmark_suite_driver(
        state,
        input.suite_id,
        input.mode,
        summary,
        driver_payload,
        interval_ms,
        None,
    )
    .await
}

pub(crate) async fn benchmark_suite_driver_status(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let status = state
        .benchmark_suite_drivers()
        .get(id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;

    Ok(benchmark_suite_driver_response_payload(
        &status.state,
        &status,
    ))
}

pub(crate) async fn benchmark_suite_driver_list(
    state: &AppState,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let drivers = state
        .benchmark_suite_drivers()
        .list_recent(MAX_BENCHMARK_SUITE_DRIVER_LIST)
        .await;

    Ok(benchmark_suite_driver_list_response_payload(&drivers))
}

pub(crate) async fn stop_benchmark_suite_driver(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let status = state
        .benchmark_suite_drivers()
        .stop(id)
        .await
        .map_err(benchmark_suite_driver_store_error_response)?;

    Ok(benchmark_suite_driver_response_payload(
        &status.state,
        &status,
    ))
}

pub(crate) fn benchmark_matrix() -> BenchmarkMatrix {
    performance::benchmark_matrix()
}

pub(crate) fn benchmark_suite_manifest(
    state: &AppState,
    id: &str,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let manifest = crate::state::benchmark_suites::load(state.config().paths(), id)
        .map_err(benchmark_suite_storage_error_response)?
        .ok_or_else(benchmark_suite_not_found_error)?;

    Ok(json!(manifest))
}

pub(crate) async fn family_c_qualification(
    state: &AppState,
    suite_id: &str,
) -> Result<serde_json::Value, LaunchApplicationError> {
    performance::family_c_qualification_payload(state, suite_id).await
}

pub(crate) fn family_c_qualification_preview() -> Result<serde_json::Value, LaunchApplicationError>
{
    performance::family_c_qualification_preview_payload()
}

struct PreparedBenchmarkSuiteDriverResume {
    previous_id: String,
    suite_id: String,
    mode: String,
    summary: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
}

pub(crate) async fn resume_benchmark_suite_driver(
    state: AppState,
    id: String,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let prepared = prepare_benchmark_suite_driver_resume(&state, &id).await?;
    start_owned_benchmark_suite_driver(
        state,
        prepared.suite_id,
        prepared.mode,
        prepared.summary,
        prepared.request,
        prepared.interval_ms,
        Some(prepared.previous_id),
    )
    .await
}

async fn prepare_benchmark_suite_driver_resume(
    state: &AppState,
    id: &str,
) -> Result<PreparedBenchmarkSuiteDriverResume, LaunchApplicationError> {
    let status = state
        .benchmark_suite_drivers()
        .get(id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;
    if !is_terminal_benchmark_suite_driver_state(&status.state) {
        return Err(benchmark_suite_driver_already_active_error());
    }

    let manifest = crate::state::benchmark_suites::load(state.config().paths(), &status.suite_id)
        .map_err(benchmark_suite_storage_error_response)?
        .ok_or_else(benchmark_suite_not_found_error)?;
    // Prefer persisted driver identity, then manifest identity, then a derived fallback.
    let suite_id = crate::state::benchmark_suites::normalize_suite_id(&status.suite_id)
        .or_else(|| crate::state::benchmark_suites::normalize_suite_id(&manifest.suite_id))
        .unwrap_or_else(|| {
            crate::state::benchmark_suites::derive_suite_id(&manifest.instance_id, &status.mode)
        });
    let mode_source = if manifest.mode.trim().is_empty() {
        status.mode.as_str()
    } else {
        manifest.mode.as_str()
    };
    let mode =
        normalize_benchmark_suite_mode(mode_source).ok_or_else(unsupported_suite_mode_error)?;
    let mut payload = BenchmarkLaunchRequest {
        instance_id: Some(manifest.instance_id.clone()),
        username: None,
        max_memory_mb: None,
        min_memory_mb: None,
        client_started_at_ms: None,
        profile: None,
        run_type: None,
        benchmark_mode: None,
        suite_mode: Some(mode),
        suite_id: Some(suite_id),
        run_index: None,
        interval_ms: Some(i64::try_from(status.interval_ms).unwrap_or(i64::MAX)),
    };
    let input = payload
        .clone()
        .into_suite_plan_input_with_manifest(Some(state.config().paths()))?;
    let summary = benchmark_suite_driver_suite_summary(&input);
    if summary.pending_run_index.is_none() {
        return Err(benchmark_suite_complete_error());
    }

    payload.suite_id = Some(input.suite_id.clone());
    payload.suite_mode = Some(input.mode.clone());
    Ok(PreparedBenchmarkSuiteDriverResume {
        previous_id: status.id,
        suite_id: input.suite_id,
        mode: input.mode,
        summary,
        request: payload,
        interval_ms: status.interval_ms,
    })
}

async fn start_owned_benchmark_suite_driver(
    state: AppState,
    suite_id: String,
    mode: String,
    summary: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    resumed_from: Option<String>,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let (ownership_tx, ownership_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let started = match state
            .benchmark_suite_drivers()
            .start(suite_id, mode, interval_ms, summary)
            .await
        {
            Ok(started) => started,
            Err(error) => {
                let _ = ownership_tx.send(Err(benchmark_suite_driver_start_error_response(error)));
                return;
            }
        };
        let mut response = benchmark_suite_driver_response_payload("scheduled", &started.status);
        if let Some(resumed_from) = resumed_from {
            response["resumed_from"] = json!(resumed_from);
        }
        let _ = ownership_tx.send(Ok(response));
        tokio::task::yield_now().await;
        own_benchmark_suite_driver_loop(
            state,
            started.status.id,
            request,
            interval_ms,
            started.effect_owner,
        )
        .await;
    });

    ownership_rx.await.unwrap_or_else(|_| {
        Err(benchmark_suite_driver_store_error_response(
            BenchmarkSuiteDriverStoreError::Persistence(io::Error::other(
                "benchmark suite driver owner stopped before reporting start",
            )),
        ))
    })
}

pub(crate) async fn launch_benchmark_suite_run(
    state: AppState,
    input: BenchmarkSuiteLaunchInput,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let BenchmarkSuiteLaunchInput {
        launch,
        suite_id,
        mode,
        run_index,
        plan,
    } = input;
    let selected = plan[run_index];
    let benchmark_id = benchmark_suite_run_id(&mode, run_index, selected);
    let mut prepared = super::prepare_launch_session(&state, launch).await?;
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(benchmark_id.as_str()),
        Some(selected.profile),
        Some(selected.run_type),
        Some(mode.as_str()),
    );
    let benchmark_response = super::launch_benchmark_status_payload(&benchmark);
    let suite_response = benchmark_suite_status_payload(&suite_id, &mode, run_index, &plan);
    prepared.task.benchmark = Some(benchmark.clone());
    let reservation = persist_benchmark_suite_run_reservation(
        state.config().paths(),
        &suite_id,
        &mode,
        &plan,
        run_index,
        &prepared.task.intent.instance_id,
        &prepared.task.intent.session_id,
        &prepared.task.launched_at,
    );
    if let Err(error) = reservation {
        finish_benchmark_suite_reservation_failure(&state, &prepared.task, benchmark).await;
        return Err(error);
    }
    let launched = super::launch_session(state.clone(), prepared.task)
        .await
        .map_err(super::launch_request_error_response)?;

    let mut response = super::launch_success_response_payload(&launched);
    response["benchmark"] = benchmark_response;
    response["suite"] = suite_response;
    Ok(response)
}

async fn finish_benchmark_suite_reservation_failure(
    state: &AppState,
    task: &super::LaunchSessionTask,
    benchmark: crate::state::launch_reports::LaunchBenchmarkMetadata,
) {
    let session_id = task.intent.session_id.as_str();
    let mut initial_evidence = super::launch_application_stage_evidence(&task.application);
    initial_evidence.extend(super::launch_boundary_stage_evidence(&task.boundary));
    state
        .sessions()
        .record_stage_evidence(session_id, initial_evidence)
        .await;
    state
        .sessions()
        .attach_benchmark(
            session_id,
            super::launch_benchmark_status_payload(&benchmark),
        )
        .await;
    state
        .sessions()
        .emit_log(
            session_id,
            "system",
            BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE.to_string(),
        )
        .await;
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: "failed".to_string(),
                benchmark: None,
                pid: None,
                exit_code: None,
                failure_class: Some("unknown".to_string()),
                failure_detail: Some(BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE.to_string()),
                healing: None,
                guardian: serde_json::to_value(&task.guardian).ok(),
                outcome: None,
                notice: None,
                evidence: vec![LaunchStageEvidence {
                    id: "application_benchmark_suite_reservation_failed".to_string(),
                    system: "application".to_string(),
                    summary: "Benchmark suite reservation failed before process start.".to_string(),
                    details: vec!["storage:benchmark_suite".to_string()],
                }],
                stages: Vec::new(),
            },
        )
        .await;

    let proof_context = LaunchProofContext::from_intent(&task.intent)
        .with_benchmark(Some(benchmark))
        .with_resource_budget(task.resource_budget.clone());
    super::runner::persist_launch_proof_for_reservation_failure(
        state,
        session_id,
        Some(task.launched_at.as_str()),
        &proof_context,
    )
    .await;
    state
        .sessions()
        .release_terminal_retention_hold(session_id)
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_benchmark_suite_run_reservation(
    paths: &axial_config::AppPaths,
    suite_id: &str,
    mode: &str,
    plan: &[BenchmarkSuiteRunSpec],
    run_index: usize,
    instance_id: &str,
    session_id: &str,
    launched_at: &str,
) -> Result<crate::state::benchmark_suites::BenchmarkSuiteManifest, LaunchApplicationError> {
    let manifest_runs = benchmark_suite_manifest_run_inputs(mode, plan);
    crate::state::benchmark_suites::persist_launched_run(
        paths,
        suite_id,
        instance_id,
        mode,
        &manifest_runs,
        run_index,
        session_id,
        launched_at,
    )
    .map_err(benchmark_suite_storage_error_response)
}

pub(crate) fn benchmark_suite_storage_error_response(
    _error: std::io::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })),
    )
}

pub(crate) fn trimmed_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(crate) fn benchmark_suite_mode_or_default(
    value: Option<&str>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok("development".to_string());
    };
    normalize_benchmark_suite_mode(value).ok_or_else(unsupported_suite_mode_error)
}

pub(crate) fn normalize_benchmark_suite_mode(value: &str) -> Option<String> {
    match value.trim() {
        "development" | "qualification" | "release_validation" => Some(value.trim().to_string()),
        _ => None,
    }
}

pub(crate) fn validate_benchmark_suite_run_index(
    run_index: i64,
    run_count: usize,
) -> Result<usize, (StatusCode, Json<serde_json::Value>)> {
    let run_index = usize::try_from(run_index).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "run_index is out of range" })),
        )
    })?;
    if run_index >= run_count {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "run_index is out of range" })),
        ));
    }

    Ok(run_index)
}

pub(crate) fn benchmark_suite_status_payload(
    suite_id: &str,
    mode: &str,
    run_index: usize,
    plan: &[BenchmarkSuiteRunSpec],
) -> serde_json::Value {
    let selected = plan[run_index];
    let remaining = plan
        .iter()
        .enumerate()
        .filter(|(index, _)| *index > run_index)
        .map(|(index, run)| benchmark_suite_run_descriptor(mode, index, *run))
        .collect::<Vec<_>>();

    json!({
        "suite_id": suite_id,
        "mode": mode,
        "run_index": run_index,
        "run_count": plan.len(),
        "selected_profile": selected.profile,
        "selected_run_type": selected.run_type,
        "selected_target_id": selected.target_id,
        "selected": benchmark_suite_run_descriptor(mode, run_index, selected),
        "remaining": remaining,
    })
}

pub(crate) fn benchmark_suite_driver_status_payload(
    suite_id: &str,
    mode: &str,
    plan: &[BenchmarkSuiteRunSpec],
    manifest: Option<&crate::state::benchmark_suites::BenchmarkSuiteManifest>,
    pending_run_index: Option<usize>,
) -> serde_json::Value {
    let launched_run_count = (0..plan.len())
        .filter(|run_index| {
            manifest
                .and_then(|manifest| {
                    manifest
                        .runs
                        .iter()
                        .find(|run| run.run_index == *run_index)
                        .and_then(|run| run.session_id.as_ref())
                })
                .is_some()
        })
        .count();

    let mut payload = json!({
        "suite_id": suite_id,
        "mode": mode,
        "run_count": plan.len(),
        "launched_run_count": launched_run_count,
        "pending_run_index": pending_run_index,
    });
    if let Some(run_index) = pending_run_index {
        payload["pending"] = benchmark_suite_run_descriptor(mode, run_index, plan[run_index]);
    }

    payload
}

pub(crate) fn benchmark_suite_driver_suite_summary(
    input: &BenchmarkSuitePlanInput,
) -> crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
    let pending_run_index = crate::state::benchmark_suites::next_pending_run_index(
        input.manifest.as_ref(),
        input.plan.len(),
    );
    let launched_run_count = (0..input.plan.len())
        .filter(|run_index| {
            input
                .manifest
                .as_ref()
                .and_then(|manifest| {
                    manifest
                        .runs
                        .iter()
                        .find(|run| run.run_index == *run_index)
                        .and_then(|run| run.session_id.as_ref())
                })
                .is_some()
        })
        .count();

    crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
        run_count: input.plan.len(),
        launched_run_count,
        pending_run_index,
    }
}

pub(crate) fn benchmark_suite_driver_response_payload(
    status: &str,
    driver: &crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus,
) -> serde_json::Value {
    let driver_id = crate::observability::bounded_descriptor_token(&driver.id, "driver");
    let driver_state = public_benchmark_suite_driver_state(&driver.state);
    let response_status = public_benchmark_suite_driver_state(status);
    let suite_id = crate::state::benchmark_suites::normalize_suite_id(&driver.suite_id)
        .filter(|normalized| normalized == &driver.suite_id)
        .unwrap_or_else(|| "suite".to_string());
    let mode =
        normalize_benchmark_suite_mode(&driver.mode).unwrap_or_else(|| "unknown".to_string());
    let created_at = public_driver_timestamp(&driver.created_at);
    let updated_at = public_driver_timestamp(&driver.updated_at);
    let run_count = driver.run_count.min(64);
    let launched_run_count = driver.launched_run_count.min(run_count);
    let pending_run_index = driver.pending_run_index.filter(|index| *index < run_count);
    let mut driver_payload = json!({
        "id": driver_id,
        "state": driver_state,
        "suite_id": suite_id,
        "mode": mode,
        "interval_ms": driver.interval_ms.clamp(
            MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS,
            MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS,
        ),
        "created_at": created_at,
        "updated_at": updated_at,
    });
    if let Some(active_session_id) = &driver.active_session_id {
        driver_payload["active_session_id"] =
            json!(bounded_status_token(active_session_id).unwrap_or_else(|| "session".to_string()));
    }
    if let Some(run_index) = driver.last_run_index.filter(|index| *index < run_count) {
        driver_payload["last_run_index"] = json!(run_index);
    }
    if let Some(session_id) = &driver.last_session_id {
        driver_payload["last_session_id"] =
            json!(bounded_status_token(session_id).unwrap_or_else(|| "session".to_string()));
    }
    if let Some(error) = &driver.error {
        driver_payload["error"] =
            json!(crate::state::benchmark_suite_drivers::sanitize_driver_error(error));
    }

    json!({
        "status": response_status,
        "driver": driver_payload,
        "suite": {
            "suite_id": suite_id,
            "mode": mode,
            "run_count": run_count,
            "launched_run_count": launched_run_count,
            "pending_run_index": pending_run_index,
        },
        "view_model": benchmark_suite_driver_view_model(driver),
    })
}

fn public_benchmark_suite_driver_state(value: &str) -> &'static str {
    match value {
        "scheduled" => "scheduled",
        "active" => "active",
        "launched_next" => "launched_next",
        "complete" => "complete",
        "failed" => "failed",
        "stopped" => "stopped",
        "interrupted" => "interrupted",
        _ => "unknown",
    }
}

fn public_driver_timestamp(value: &str) -> String {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| {
            value
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn benchmark_suite_driver_list_response_payload(
    drivers: &[crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus],
) -> serde_json::Value {
    json!({
        "status": "ok",
        "drivers": drivers
            .iter()
            .map(|driver| benchmark_suite_driver_response_payload(&driver.state, driver))
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn clamp_benchmark_suite_driver_interval_ms(value: Option<i64>) -> u64 {
    let Some(value) = value else {
        return DEFAULT_BENCHMARK_SUITE_DRIVER_INTERVAL_MS;
    };
    if value <= MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS as i64 {
        return MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS;
    }
    if value >= MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS as i64 {
        return MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS;
    }

    value as u64
}

pub(crate) fn is_terminal_benchmark_suite_driver_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "complete" | "failed" | "stopped" | "interrupted"
    )
}

fn benchmark_suite_driver_view_model(
    driver: &crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus,
) -> serde_json::Value {
    let state = public_benchmark_suite_driver_state(&driver.state);
    json!({
        "state_label": benchmark_suite_driver_state_label(state),
        "state_tone": benchmark_suite_driver_state_tone(state),
        "can_stop": !is_terminal_benchmark_suite_driver_state(state),
        "can_resume": is_restartable_benchmark_suite_driver_state(state),
        "can_check_family_c_qualification": can_check_family_c_qualification(driver),
    })
}

fn benchmark_suite_driver_state_label(state: &str) -> String {
    public_token_label(state, "Unknown")
}

fn benchmark_suite_driver_state_tone(state: &str) -> &'static str {
    match state.trim().to_ascii_lowercase().as_str() {
        "complete" => "ok",
        "failed" => "err",
        "stopped" | "interrupted" => "warn",
        "active" => "accent",
        "scheduled" | "launched_next" => "info",
        _ => "neutral",
    }
}

fn is_restartable_benchmark_suite_driver_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "failed" | "stopped" | "interrupted"
    )
}

fn can_check_family_c_qualification(
    driver: &crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus,
) -> bool {
    !driver.suite_id.trim().is_empty()
        && driver
            .mode
            .trim()
            .eq_ignore_ascii_case(performance::FAMILY_C_QUALIFICATION_MODE)
}

fn public_token_label(value: &str, fallback: &str) -> String {
    let labels = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    if labels.is_empty() {
        fallback.to_string()
    } else {
        labels.join(" ")
    }
}

pub(crate) fn benchmark_suite_not_found_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "benchmark suite not found" })),
    )
}

pub(crate) fn benchmark_suite_driver_not_found_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "benchmark suite driver not found" })),
    )
}

pub(crate) fn benchmark_suite_driver_already_active_error() -> (StatusCode, Json<serde_json::Value>)
{
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite driver is already active" })),
    )
}

fn benchmark_suite_driver_start_error_response(
    error: BenchmarkSuiteDriverStartError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        BenchmarkSuiteDriverStartError::Conflict => benchmark_suite_driver_already_active_error(),
        BenchmarkSuiteDriverStartError::Store { source, .. } => {
            benchmark_suite_driver_store_error_response(source)
        }
    }
}

fn benchmark_suite_driver_store_error_response(
    error: BenchmarkSuiteDriverStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        BenchmarkSuiteDriverStoreError::MissingDriver => benchmark_suite_driver_not_found_error(),
        BenchmarkSuiteDriverStoreError::TerminalDriver => benchmark_suite_driver_terminal_error(),
        BenchmarkSuiteDriverStoreError::RetryRequired
        | BenchmarkSuiteDriverStoreError::RetryUnavailable
        | BenchmarkSuiteDriverStoreError::Persistence(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "benchmark suite driver state could not be persisted"
            })),
        ),
    }
}

fn benchmark_suite_driver_terminal_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "benchmark suite driver is already terminal"
        })),
    )
}

pub(crate) fn benchmark_suite_complete_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite is complete" })),
    )
}

pub(crate) fn benchmark_suite_active_run_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite has active run" })),
    )
}

pub(crate) fn unsupported_suite_mode_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "suite_mode is not supported" })),
    )
}

pub(crate) async fn ensure_no_active_benchmark_suite_auto_run(
    sessions: &crate::state::SessionStore,
    manifest: Option<&crate::state::benchmark_suites::BenchmarkSuiteManifest>,
    auto_next_run: bool,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if !auto_next_run {
        return Ok(());
    }

    let Some(manifest) = manifest else {
        return Ok(());
    };
    if active_benchmark_suite_session_id(sessions, manifest)
        .await
        .is_some()
    {
        return Err(benchmark_suite_active_run_error());
    }

    Ok(())
}

pub(crate) async fn benchmark_suite_driver_decision(
    sessions: &crate::state::SessionStore,
    input: BenchmarkSuitePlanInput,
) -> Result<BenchmarkSuiteDriverDecision, (StatusCode, Json<serde_json::Value>)> {
    // The driver either reports the active run, completes, or schedules exactly one next run.
    let pending_run_index = crate::state::benchmark_suites::next_pending_run_index(
        input.manifest.as_ref(),
        input.plan.len(),
    );
    let suite = benchmark_suite_driver_status_payload(
        &input.suite_id,
        &input.mode,
        &input.plan,
        input.manifest.as_ref(),
        pending_run_index,
    );

    if let Some(manifest) = input.manifest.as_ref()
        && let Some(active_session_id) = active_benchmark_suite_session_id(sessions, manifest).await
    {
        return Ok(BenchmarkSuiteDriverDecision::Active {
            suite,
            active_session_id,
        });
    }

    let Some(run_index) = pending_run_index else {
        return Ok(BenchmarkSuiteDriverDecision::Complete { suite });
    };

    Ok(BenchmarkSuiteDriverDecision::Launch(
        BenchmarkSuiteLaunchInput {
            launch: input.launch,
            suite_id: input.suite_id,
            mode: input.mode,
            run_index,
            plan: input.plan,
        },
    ))
}

pub(crate) fn spawn_benchmark_suite_driver_loop(
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    effect_owner: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverEffectOwner,
) {
    tokio::spawn(async move {
        own_benchmark_suite_driver_loop(state, driver_id, request, interval_ms, effect_owner).await;
    });
}

async fn own_benchmark_suite_driver_loop(
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    effect_owner: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverEffectOwner,
) {
    let stop_rx = effect_owner.stop_receiver();
    match run_benchmark_suite_driver_loop(state, driver_id, request, interval_ms, stop_rx).await {
        Ok(()) | Err(BenchmarkSuiteDriverStoreError::TerminalDriver) => {}
        Err(error) => tracing::warn!(
            error_class = error.class(),
            "benchmark suite driver persistence failed"
        ),
    }
    drop(effect_owner);
}

pub(crate) async fn run_benchmark_suite_driver_loop(
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), BenchmarkSuiteDriverStoreError> {
    // Stop requests are observed between launches so an in-flight benchmark can finish cleanly.
    loop {
        if *stop_rx.borrow() {
            break;
        }

        let input = match request
            .clone()
            .into_suite_plan_input_with_manifest(Some(state.config().paths()))
        {
            Ok(input) => input,
            Err(error) => {
                state
                    .benchmark_suite_drivers()
                    .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                    .await?;
                break;
            }
        };
        let summary = benchmark_suite_driver_suite_summary(&input);

        match benchmark_suite_driver_decision(state.sessions().as_ref(), input).await {
            Ok(BenchmarkSuiteDriverDecision::Active {
                active_session_id, ..
            }) => {
                state
                    .benchmark_suite_drivers()
                    .record_active(&driver_id, summary, Some(active_session_id))
                    .await?;
            }
            Ok(BenchmarkSuiteDriverDecision::Complete { .. }) => {
                state
                    .benchmark_suite_drivers()
                    .record_complete(&driver_id, summary)
                    .await?;
                break;
            }
            Ok(BenchmarkSuiteDriverDecision::Launch(input)) => {
                if *stop_rx.borrow() {
                    break;
                }
                let run_index = input.run_index;
                match launch_benchmark_suite_run(state.clone(), input).await {
                    Ok(payload) => {
                        let session_id = payload
                            .get("session_id")
                            .and_then(|value| value.as_str())
                            .and_then(bounded_status_token);
                        let summary = request
                            .clone()
                            .into_suite_plan_input_with_manifest(Some(state.config().paths()))
                            .map(|input| benchmark_suite_driver_suite_summary(&input))
                            .unwrap_or(summary);
                        state
                            .benchmark_suite_drivers()
                            .record_launched(&driver_id, summary, run_index, session_id)
                            .await?;
                    }
                    Err(error) => {
                        state
                            .benchmark_suite_drivers()
                            .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                            .await?;
                        break;
                    }
                }
            }
            Err(error) => {
                state
                    .benchmark_suite_drivers()
                    .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                    .await?;
                break;
            }
        }

        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
        }
    }
    Ok(())
}

pub(crate) fn benchmark_suite_api_error_message(
    error: &(StatusCode, Json<serde_json::Value>),
) -> String {
    error
        .1
        .0
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("benchmark suite driver failed")
        .to_string()
}

pub(crate) async fn active_benchmark_suite_session_id(
    sessions: &crate::state::SessionStore,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
) -> Option<String> {
    for session_id in manifest
        .runs
        .iter()
        .filter_map(|run| run.session_id.as_deref())
    {
        let Some(record) = sessions.get(session_id).await else {
            continue;
        };
        if !matches!(record.state, LaunchState::Failed | LaunchState::Exited) {
            return Some(
                bounded_status_token(&record.session_id.0)
                    .unwrap_or_else(|| "active-session".to_string()),
            );
        }
    }

    None
}

pub(crate) fn bounded_status_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(crate::observability::bounded_descriptor_token(
        value, "session",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use axial_launcher::{LaunchSessionRecord, SessionId};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn benchmark_suite_reservation_failure_finalizes_and_releases_prepared_session() {
        let fixture = BenchmarkFixture::new("reservation-finalizes-session");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Benchmark", "1.21.1");
        let suite_id = "reservation-finalizes-session";
        let corrupt_manifest = crate::state::benchmark_suites::suite_path(&fixture.paths, suite_id);
        fs::create_dir_all(corrupt_manifest.parent().expect("suite parent"))
            .expect("create suite parent");
        fs::write(&corrupt_manifest, b"{not-json").expect("corrupt suite manifest");
        let plan = performance::benchmark_suite_plan("development").expect("development plan");

        let error = launch_benchmark_suite_run(
            fixture.state.clone(),
            BenchmarkSuiteLaunchInput {
                launch: super::super::LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                suite_id: suite_id.to_string(),
                mode: "development".to_string(),
                run_index: 0,
                plan,
            },
        )
        .await
        .expect_err("corrupt suite manifest should reject reservation");

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error.1.0,
            json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })
        );
        let proofs = crate::state::launch_reports::list_recent(&fixture.paths, 5)
            .expect("list launch proofs");
        assert_eq!(proofs.len(), 1);
        let proof = &proofs[0];
        assert_eq!(proof.outcome, "failed");
        assert_eq!(
            proof.scenario.benchmark_mode.as_deref(),
            Some("development")
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some(BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE)
        );
        assert!(proof.stages.iter().any(|stage| {
            stage.evidence.iter().any(|evidence| {
                evidence.id == "application_benchmark_suite_reservation_failed"
                    && evidence.system == "application"
            })
        }));

        let session_id = proof.session_id.clone();
        let record = fixture
            .state
            .sessions()
            .get(&session_id)
            .await
            .expect("terminal prepared session");
        assert_eq!(record.state, LaunchState::Failed);
        assert!(fixture.state.sessions().active_records().await.is_empty());

        for index in 0..=32 {
            let completed_id = format!("completed-{index}");
            fixture
                .state
                .sessions()
                .insert(test_record(&completed_id))
                .await;
            fixture
                .state
                .sessions()
                .release_terminal_retention_hold(&completed_id)
                .await;
            fixture
                .state
                .sessions()
                .emit_status(&completed_id, terminal_status())
                .await;
        }
        assert!(fixture.state.sessions().get(&session_id).await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceled_start_waiter_does_not_drop_detached_effect_owner() {
        let fixture = BenchmarkFixture::new("canceled-driver-start-waiter");
        let state = fixture.state.clone();
        let mut waiter = Box::pin(start_owned_benchmark_suite_driver(
            state.clone(),
            "detached-suite".to_string(),
            "development".to_string(),
            crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                run_count: 2,
                launched_run_count: 0,
                pending_run_index: Some(0),
            },
            BenchmarkLaunchRequest {
                instance_id: Some("missing-instance".to_string()),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
                profile: None,
                run_type: None,
                benchmark_mode: None,
                suite_mode: Some("development".to_string()),
                suite_id: Some("detached-suite".to_string()),
                run_index: None,
                interval_ms: Some(30_000),
            },
            30_000,
            None,
        ));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(waiter.as_mut().poll(&mut context), Poll::Pending));
        drop(waiter);

        let failed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(status) = state
                    .benchmark_suite_drivers()
                    .list_recent(1)
                    .await
                    .into_iter()
                    .next()
                    && status.state == "failed"
                {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached owner terminalizes driver");
        assert_eq!(failed.suite_id, "detached-suite");

        let successor = state
            .benchmark_suite_drivers()
            .start(
                "detached-suite".to_string(),
                "development".to_string(),
                30_000,
                crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 0,
                    pending_run_index: Some(0),
                },
            )
            .await
            .expect("effect owner releases only after detached loop exits");
        drop(successor.effect_owner);
    }

    struct BenchmarkFixture {
        state: AppState,
        paths: AppPaths,
        root: PathBuf,
    }

    impl BenchmarkFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            fs::create_dir_all(&paths.library_dir).expect("create library dir");
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            config
                .replace_in_memory(AppConfig {
                    library_dir: paths.library_dir.to_string_lossy().to_string(),
                    ..AppConfig::default()
                })
                .expect("set library dir");
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                startup_warnings: Vec::new(),
                frontend_dir: root.join("frontend"),
            });
            Self { state, paths, root }
        }

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            self.state
                .instances()
                .add(
                    name.to_string(),
                    version_id.to_string(),
                    String::new(),
                    String::new(),
                    None,
                )
                .expect("add instance")
                .id
        }

        fn write_ready_install(&self, version_id: &str) {
            let version_dir = self.paths.library_dir.join("versions").join(version_id);
            fs::create_dir_all(&version_dir).expect("version dir");
            fs::write(
                version_dir.join(format!("{version_id}.json")),
                serde_json::to_vec(&json!({
                    "id": version_id,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "javaVersion": {
                        "component": "java-runtime-delta",
                        "majorVersion": 21
                    },
                    "libraries": []
                }))
                .expect("version json"),
            )
            .expect("write version json");
            fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
                .expect("write client jar");

            let runtime_bin = self
                .paths
                .library_dir
                .join("runtime")
                .join("java-runtime-delta")
                .join("bin");
            fs::create_dir_all(&runtime_bin).expect("runtime bin");
            let java_name = if cfg!(target_os = "windows") {
                "javaw.exe"
            } else {
                "java"
            };
            let java_path = runtime_bin.join(java_name);
            fs::write(&java_path, b"java").expect("runtime java");
            make_executable(&java_path);
        }
    }

    impl Drop for BenchmarkFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
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
            outcome: None,
            stages: Vec::new(),
        }
    }

    fn terminal_status() -> LaunchStatusEvent {
        LaunchStatusEvent {
            state: "exited".to_string(),
            benchmark: None,
            pid: None,
            exit_code: Some(0),
            failure_class: None,
            failure_detail: None,
            healing: None,
            guardian: None,
            outcome: None,
            notice: None,
            evidence: Vec::new(),
            stages: Vec::new(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("axial-benchmark-{name}-{nanos}"))
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

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("set executable");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}
}
