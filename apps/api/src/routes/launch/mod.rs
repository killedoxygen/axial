mod matrix;
mod policy;
mod runner;
mod stream;
mod task;

use crate::state::{AppState, LaunchStatusEvent};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_launcher::{LaunchState, snapshot_status};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const DEFAULT_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 30_000;
const MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 5_000;
const MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS: u64 = 3_600_000;
const MAX_BENCHMARK_SUITE_DRIVER_LIST: usize = 25;
const FAMILY_C_QUALIFICATION_PROOF_SCAN_LIMIT: usize = 100;
const FAMILY_C_QUALIFICATION_SCHEMA: &str =
    "croopor.launch.benchmark.qualification.family_c_1_12_2";
const FAMILY_C_QUALIFICATION_SCHEMA_VERSION: u32 = 1;
const FAMILY_C_QUALIFICATION_MODE: &str = "release_validation";
const FAMILY_C_QUALIFICATION_VERSION: &str = "1.12.2";
const FAMILY_C_QUALIFICATION_LOADER: &str = "Forge";
const FAMILY_C_BASELINE_TARGET_ID: &str = "family_c_forge_1_12_2_vanilla_baseline";
const FAMILY_C_MANAGED_TARGET_ID: &str = "family_c_forge_1_12_2_family_c_forge_core";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/launch", post(handle_launch))
        .route("/api/v1/launch/benchmark", post(handle_benchmark_launch))
        .route(
            "/api/v1/launch/benchmark/suite",
            post(handle_benchmark_suite_launch),
        )
        .route(
            "/api/v1/launch/benchmark/suite/tick",
            post(handle_benchmark_suite_tick),
        )
        .route(
            "/api/v1/launch/benchmark/suite/driver",
            post(handle_benchmark_suite_driver_start),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers",
            get(handle_benchmark_suite_driver_list),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}",
            get(handle_benchmark_suite_driver_status),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/stop",
            post(handle_benchmark_suite_driver_stop),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/resume",
            post(handle_benchmark_suite_driver_resume),
        )
        .route(
            "/api/v1/launch/benchmark/suites/{id}",
            get(handle_benchmark_suite_manifest),
        )
        .route(
            "/api/v1/launch/benchmark/qualification/family-c-1-12-2/preview",
            get(handle_family_c_qualification_preview),
        )
        .route(
            "/api/v1/launch/benchmark/qualification/family-c-1-12-2/{suite_id}",
            get(handle_family_c_qualification),
        )
        .route(
            "/api/v1/launch/benchmark/matrix",
            get(handle_benchmark_matrix),
        )
        .route("/api/v1/launch/reports", get(handle_launch_reports))
        .route("/api/v1/launch/reports/{id}", get(handle_launch_report))
        .route("/api/v1/launch/{id}/events", get(handle_launch_events))
        .route("/api/v1/launch/{id}/status", get(handle_launch_status))
        .route("/api/v1/launch/{id}/command", get(handle_launch_command))
        .route("/api/v1/launch/{id}/kill", post(handle_launch_kill))
}

async fn handle_launch(
    State(state): State<AppState>,
    Json(payload): Json<task::LaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let prepared = task::prepare_launch_session(&state, payload).await?;
    let launched = runner::launch_session(state.clone(), prepared.task)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error.message,
                    "healing": error.healing,
                    "guardian": error.guardian,
                })),
            )
        })?;

    Ok(Json(launch_success_response_payload(&launched)))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BenchmarkLaunchRequest {
    #[serde(default)]
    instance_id: Option<String>,
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
struct BenchmarkLaunchInput {
    launch: task::LaunchRequest,
    profile: Option<String>,
    run_type: Option<String>,
    benchmark_mode: Option<String>,
}

#[derive(Debug)]
struct BenchmarkSuiteLaunchInput {
    launch: task::LaunchRequest,
    suite_id: String,
    mode: String,
    run_index: usize,
    plan: Vec<matrix::BenchmarkSuiteRunSpec>,
}

#[derive(Debug)]
struct BenchmarkSuitePlanInput {
    launch: task::LaunchRequest,
    suite_id: String,
    mode: String,
    plan: Vec<matrix::BenchmarkSuiteRunSpec>,
    manifest: Option<crate::state::benchmark_suites::BenchmarkSuiteManifest>,
}

#[derive(Debug)]
enum BenchmarkSuiteDriverDecision {
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
        let summary = resume_restart_interrupted_benchmark_suite_drivers(state).await;
        if summary.pending > 0 {
            tracing::info!(
                pending = summary.pending,
                resumed = summary.resumed,
                failed = summary.failed,
                "benchmark suite drivers resumed after restart"
            );
        }
    });
    true
}

pub(crate) async fn resume_restart_interrupted_benchmark_suite_drivers(
    state: AppState,
) -> BenchmarkSuiteDriverResumeSummary {
    let pending = state
        .benchmark_suite_drivers()
        .take_restart_interrupted_resumable_drivers()
        .await;
    let mut summary = BenchmarkSuiteDriverResumeSummary {
        pending: pending.len(),
        ..BenchmarkSuiteDriverResumeSummary::default()
    };

    for status in pending {
        match resume_benchmark_suite_driver(state.clone(), status.id.clone()).await {
            Ok(_) => {
                summary.resumed += 1;
                state
                    .benchmark_suite_drivers()
                    .record_restart_resume_started(&status.id)
                    .await;
            }
            Err(error) => {
                summary.failed += 1;
                state
                    .benchmark_suite_drivers()
                    .record_restart_resume_failed(
                        &status.id,
                        &benchmark_suite_api_error_message(&error),
                    )
                    .await;
            }
        }
    }

    summary
}

impl BenchmarkLaunchRequest {
    fn into_launch_input(
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
    fn into_suite_launch_input(
        self,
    ) -> Result<BenchmarkSuiteLaunchInput, (StatusCode, Json<serde_json::Value>)> {
        self.into_suite_launch_input_with_manifest(None)
    }

    fn into_suite_launch_input_with_manifest(
        self,
        paths: Option<&croopor_config::AppPaths>,
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

    fn into_suite_plan_input_with_manifest(
        self,
        paths: Option<&croopor_config::AppPaths>,
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
        let plan = matrix::benchmark_suite_plan(&mode).ok_or_else(unsupported_suite_mode_error)?;
        let manifest = match paths {
            Some(paths) => {
                crate::state::benchmark_suites::load(paths, &suite_id).map_err(internal_error)?
            }
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

    fn launch_request(&self) -> Result<task::LaunchRequest, (StatusCode, Json<serde_json::Value>)> {
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

        Ok(task::LaunchRequest {
            instance_id,
            username: self.username.clone(),
            max_memory_mb: self.max_memory_mb,
            min_memory_mb: self.min_memory_mb,
            client_started_at_ms: self.client_started_at_ms,
        })
    }
}

async fn handle_benchmark_launch(
    State(state): State<AppState>,
    Json(payload): Json<BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let input = payload.into_launch_input()?;
    let mut prepared = task::prepare_launch_session(&state, input.launch).await?;
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(prepared.task.intent.session_id.as_str()),
        input.profile.as_deref(),
        input.run_type.as_deref(),
        input.benchmark_mode.as_deref(),
    );
    let benchmark_response = benchmark_status_payload(&benchmark);
    prepared.task.benchmark = Some(benchmark.clone());
    let launched = runner::launch_session(state.clone(), prepared.task)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error.message,
                    "healing": error.healing,
                    "guardian": error.guardian,
                })),
            )
        })?;

    let mut response = launch_success_response_payload(&launched);
    response["benchmark"] = benchmark_response;
    Ok(Json(response))
}

async fn handle_benchmark_suite_launch(
    State(state): State<AppState>,
    Json(payload): Json<BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let auto_next_run = payload.run_index.is_none();
    if auto_next_run {
        let launch = payload.launch_request()?;
        let mode = benchmark_suite_mode_or_default(payload.suite_mode.as_deref())?;
        let _ = matrix::benchmark_suite_plan(&mode).ok_or_else(unsupported_suite_mode_error)?;
        let suite_id = payload
            .suite_id
            .as_deref()
            .and_then(crate::state::benchmark_suites::normalize_suite_id)
            .unwrap_or_else(|| {
                crate::state::benchmark_suites::derive_suite_id(&launch.instance_id, &mode)
            });
        let manifest = crate::state::benchmark_suites::load(state.config().paths(), &suite_id)
            .map_err(internal_error)?;
        ensure_no_active_benchmark_suite_auto_run(
            state.sessions().as_ref(),
            manifest.as_ref(),
            auto_next_run,
        )
        .await?;
    }

    let input = payload.into_suite_launch_input_with_manifest(Some(state.config().paths()))?;
    launch_benchmark_suite_run(state, input).await.map(Json)
}

async fn handle_benchmark_suite_tick(
    State(state): State<AppState>,
    Json(payload): Json<BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let input = payload.into_suite_plan_input_with_manifest(Some(state.config().paths()))?;
    match benchmark_suite_driver_decision(state.sessions().as_ref(), input).await? {
        BenchmarkSuiteDriverDecision::Active {
            suite,
            active_session_id,
        } => Ok(Json(json!({
            "status": "active",
            "driver": { "state": "active" },
            "suite": suite,
            "active_session_id": active_session_id,
        }))),
        BenchmarkSuiteDriverDecision::Complete { suite } => Ok(Json(json!({
            "status": "complete",
            "driver": { "state": "complete" },
            "suite": suite,
        }))),
        BenchmarkSuiteDriverDecision::Launch(input) => {
            let mut payload = launch_benchmark_suite_run(state, input).await?;
            payload["driver"] = json!({ "state": "launched_next" });
            Ok(Json(payload))
        }
    }
}

async fn handle_benchmark_suite_driver_start(
    State(state): State<AppState>,
    Json(payload): Json<BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let interval_ms = clamp_benchmark_suite_driver_interval_ms(payload.interval_ms);
    let input = payload
        .clone()
        .into_suite_plan_input_with_manifest(Some(state.config().paths()))?;
    let summary = benchmark_suite_driver_suite_summary(&input);
    let mut driver_payload = payload.clone();
    driver_payload.suite_id = Some(input.suite_id.clone());
    driver_payload.suite_mode = Some(input.mode.clone());
    driver_payload.benchmark_mode = None;
    driver_payload.run_index = None;

    let started = state
        .benchmark_suite_drivers()
        .start(input.suite_id, input.mode, interval_ms, summary)
        .await
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                Json(json!({ "error": "benchmark suite driver is already active" })),
            )
        })?;
    spawn_benchmark_suite_driver_loop(
        state,
        started.status.id.clone(),
        driver_payload,
        interval_ms,
        started.stop_rx,
    );

    Ok(Json(benchmark_suite_driver_response_payload(
        "scheduled",
        &started.status,
    )))
}

async fn handle_benchmark_suite_driver_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let status = state
        .benchmark_suite_drivers()
        .get(&id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;

    Ok(Json(benchmark_suite_driver_response_payload(
        &status.state,
        &status,
    )))
}

async fn handle_benchmark_suite_driver_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let drivers = state
        .benchmark_suite_drivers()
        .list_recent(MAX_BENCHMARK_SUITE_DRIVER_LIST)
        .await;

    Ok(Json(benchmark_suite_driver_list_response_payload(&drivers)))
}

async fn handle_benchmark_suite_driver_stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let status = state
        .benchmark_suite_drivers()
        .stop(&id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;

    Ok(Json(benchmark_suite_driver_response_payload(
        &status.state,
        &status,
    )))
}

async fn handle_benchmark_suite_driver_resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    resume_benchmark_suite_driver(state, id).await.map(Json)
}

async fn resume_benchmark_suite_driver(
    state: AppState,
    id: String,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let status = state
        .benchmark_suite_drivers()
        .get(&id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;
    if !is_terminal_benchmark_suite_driver_state(&status.state) {
        return Err(benchmark_suite_driver_already_active_error());
    }

    let manifest = crate::state::benchmark_suites::load(state.config().paths(), &status.suite_id)
        .map_err(internal_error)?
        .ok_or_else(benchmark_suite_not_found_error)?;
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
    let started = state
        .benchmark_suite_drivers()
        .start(input.suite_id, input.mode, status.interval_ms, summary)
        .await
        .map_err(|_| benchmark_suite_driver_already_active_error())?;
    spawn_benchmark_suite_driver_loop(
        state,
        started.status.id.clone(),
        payload,
        status.interval_ms,
        started.stop_rx,
    );

    let mut response = benchmark_suite_driver_response_payload("scheduled", &started.status);
    response["resumed_from"] = json!(status.id);
    Ok(response)
}

async fn launch_benchmark_suite_run(
    state: AppState,
    input: BenchmarkSuiteLaunchInput,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let selected = input.plan[input.run_index];
    let benchmark_id = benchmark_suite_run_id(&input.mode, input.run_index, selected);
    let mut prepared = task::prepare_launch_session(&state, input.launch).await?;
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(benchmark_id.as_str()),
        Some(selected.profile),
        Some(selected.run_type),
        Some(input.mode.as_str()),
    );
    let benchmark_response = benchmark_status_payload(&benchmark);
    let suite_response =
        benchmark_suite_status_payload(&input.suite_id, &input.mode, input.run_index, &input.plan);
    prepared.task.benchmark = Some(benchmark.clone());
    let launched = runner::launch_session(state.clone(), prepared.task)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": error.message,
                    "healing": error.healing,
                    "guardian": error.guardian,
                })),
            )
        })?;
    let manifest_runs = benchmark_suite_manifest_run_inputs(&input.mode, &input.plan);
    crate::state::benchmark_suites::persist_launched_run(
        state.config().paths(),
        &input.suite_id,
        &launched.instance_id,
        &input.mode,
        &manifest_runs,
        input.run_index,
        &launched.session_id,
        &launched.launched_at,
    )
    .map_err(internal_error)?;

    let mut response = launch_success_response_payload(&launched);
    response["benchmark"] = benchmark_response;
    response["suite"] = suite_response;
    Ok(response)
}

async fn handle_benchmark_matrix() -> Json<matrix::BenchmarkMatrix> {
    Json(matrix::benchmark_matrix())
}

async fn handle_benchmark_suite_manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let manifest = crate::state::benchmark_suites::load(state.config().paths(), &id)
        .map_err(internal_error)?
        .ok_or_else(benchmark_suite_not_found_error)?;

    Ok(Json(json!(manifest)))
}

async fn handle_family_c_qualification(
    State(state): State<AppState>,
    Path(suite_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    family_c_qualification_payload(&state, &suite_id)
        .await
        .map(Json)
}

async fn handle_family_c_qualification_preview()
-> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    family_c_qualification_preview_payload().map(Json)
}

async fn handle_launch_reports(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let reports = crate::state::launch_reports::list_recent(state.config().paths(), 25)
        .map_err(internal_error)?;

    Ok(Json(json!({ "reports": reports })))
}

async fn handle_launch_report(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let report = crate::state::launch_reports::load(state.config().paths(), &id)
        .map_err(internal_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "launch report not found" })),
            )
        })?;

    Ok(Json(json!(report)))
}

async fn handle_launch_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, Json<serde_json::Value>),
> {
    stream::launch_events_sse(state, id).await
}

async fn handle_launch_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    Ok(Json(json!({
        "command": record.command,
        "java_path": record.java_path,
        "session_id": record.session_id.0,
        "healing": record.healing,
        "guardian": record.guardian,
    })))
}

async fn handle_launch_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    let status = snapshot_status(&record);
    let mut response = json!({
        "state": status.state,
        "pid": status.pid,
        "exit_code": status.exit_code,
        "failure_class": status.failure_class,
        "failure_detail": status.failure_detail,
        "healing": status.healing,
        "guardian": status.guardian,
        "stages": status.stages,
        "session_id": record.session_id.0,
    });
    if let Some(benchmark) = status.benchmark {
        response["benchmark"] = benchmark;
    }

    Ok(Json(response))
}

async fn handle_launch_kill(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let record = state.sessions().get(&id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    })?;

    state.sessions().kill(&id).await.map_err(|error| {
        let status = if error.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(json!({ "error": error.to_string() })))
    })?;

    runner::trace_launch_event(&id, "kill requested by client");
    state
        .sessions()
        .emit_log(&id, "system", "Launch stopped by user.".to_string())
        .await;
    state
        .sessions()
        .emit_status(
            &id,
            LaunchStatusEvent {
                state: "exited".to_string(),
                benchmark: None,
                pid: record.pid,
                exit_code: Some(-9),
                failure_class: None,
                failure_detail: Some("stopped by user".to_string()),
                healing: record.healing.clone(),
                guardian: record.guardian.clone(),
                stages: Vec::new(),
            },
        )
        .await;
    runner::persist_launch_proof_best_effort(&state, &id, record.launched_at.as_deref(), "stopped")
        .await;

    Ok(Json(json!({ "status": "killed" })))
}

fn internal_error(error: std::io::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": error.to_string() })),
    )
}

fn trimmed_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn benchmark_status_payload(
    benchmark: &crate::state::launch_reports::LaunchBenchmarkMetadata,
) -> serde_json::Value {
    let mut payload = json!({
        "id": benchmark.benchmark_id,
        "profile": benchmark.profile,
        "run_type": benchmark.run_type,
    });
    if let Some(mode) = &benchmark.mode {
        payload["mode"] = json!(mode);
    }
    payload
}

fn launch_success_response_payload(launched: &runner::LaunchSuccess) -> serde_json::Value {
    json!({
        "status": "launching",
        "session_id": &launched.session_id,
        "instance_id": &launched.instance_id,
        "pid": launched.pid,
        "launched_at": &launched.launched_at,
        "max_memory_mb": launched.max_memory_mb,
        "min_memory_mb": launched.min_memory_mb,
        "healing": &launched.healing,
        "guardian": &launched.guardian,
    })
}

fn benchmark_suite_mode_or_default(
    value: Option<&str>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok("development".to_string());
    };
    normalize_benchmark_suite_mode(value).ok_or_else(unsupported_suite_mode_error)
}

fn normalize_benchmark_suite_mode(value: &str) -> Option<String> {
    match value.trim() {
        "development" | "qualification" | "release_validation" => Some(value.trim().to_string()),
        _ => None,
    }
}

fn validate_benchmark_suite_run_index(
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

fn benchmark_suite_run_id(
    mode: &str,
    run_index: usize,
    run: matrix::BenchmarkSuiteRunSpec,
) -> String {
    match run.target_id {
        Some(target_id) => format!("suite-{mode}-{run_index:02}-{target_id}-{}", run.run_type),
        None => format!(
            "suite-{mode}-{run_index:02}-{}-{}",
            run.profile, run.run_type
        ),
    }
}

fn benchmark_suite_run_descriptor(
    mode: &str,
    run_index: usize,
    run: matrix::BenchmarkSuiteRunSpec,
) -> serde_json::Value {
    json!({
        "run_index": run_index,
        "profile": run.profile,
        "run_type": run.run_type,
        "target_id": run.target_id,
        "benchmark_id": benchmark_suite_run_id(mode, run_index, run),
    })
}

fn benchmark_suite_manifest_run_inputs(
    mode: &str,
    plan: &[matrix::BenchmarkSuiteRunSpec],
) -> Vec<crate::state::benchmark_suites::BenchmarkSuiteRunInput> {
    plan.iter()
        .enumerate()
        .map(
            |(index, run)| crate::state::benchmark_suites::BenchmarkSuiteRunInput {
                run_index: index,
                profile: run.profile.to_string(),
                run_type: run.run_type.to_string(),
                target_id: run.target_id.map(str::to_string),
                benchmark_id: benchmark_suite_run_id(mode, index, *run),
            },
        )
        .collect()
}

fn benchmark_suite_status_payload(
    suite_id: &str,
    mode: &str,
    run_index: usize,
    plan: &[matrix::BenchmarkSuiteRunSpec],
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

fn benchmark_suite_driver_status_payload(
    suite_id: &str,
    mode: &str,
    plan: &[matrix::BenchmarkSuiteRunSpec],
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

fn benchmark_suite_driver_suite_summary(
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

fn benchmark_suite_driver_response_payload(
    status: &str,
    driver: &crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus,
) -> serde_json::Value {
    let mut driver_payload = json!({
        "id": driver.id,
        "state": driver.state,
        "suite_id": driver.suite_id,
        "mode": driver.mode,
        "interval_ms": driver.interval_ms,
        "created_at": driver.created_at,
        "updated_at": driver.updated_at,
    });
    if let Some(active_session_id) = &driver.active_session_id {
        driver_payload["active_session_id"] = json!(active_session_id);
    }
    if let Some(run_index) = driver.last_run_index {
        driver_payload["last_run_index"] = json!(run_index);
    }
    if let Some(session_id) = &driver.last_session_id {
        driver_payload["last_session_id"] = json!(session_id);
    }
    if let Some(error) = &driver.error {
        driver_payload["error"] = json!(error);
    }

    json!({
        "status": status,
        "driver": driver_payload,
        "suite": {
            "suite_id": driver.suite_id,
            "mode": driver.mode,
            "run_count": driver.run_count,
            "launched_run_count": driver.launched_run_count,
            "pending_run_index": driver.pending_run_index,
        },
    })
}

fn benchmark_suite_driver_list_response_payload(
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

#[derive(Clone, Copy)]
struct FamilyCQualificationTarget {
    role: &'static str,
    target_id: &'static str,
    profile: &'static str,
    run_type: &'static str,
    performance_mode: &'static str,
    comparison_required: bool,
}

fn family_c_qualification_targets() -> [FamilyCQualificationTarget; 2] {
    [
        FamilyCQualificationTarget {
            role: "baseline",
            target_id: FAMILY_C_BASELINE_TARGET_ID,
            profile: "vanilla_baseline",
            run_type: "coldish",
            performance_mode: "vanilla",
            comparison_required: false,
        },
        FamilyCQualificationTarget {
            role: "managed",
            target_id: FAMILY_C_MANAGED_TARGET_ID,
            profile: "managed_default",
            run_type: "coldish",
            performance_mode: "managed",
            comparison_required: true,
        },
    ]
}

async fn family_c_qualification_payload(
    state: &AppState,
    suite_id: &str,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let normalized_suite_id = crate::state::benchmark_suites::normalize_suite_id(suite_id)
        .ok_or_else(benchmark_suite_not_found_error)?;
    let manifest =
        crate::state::benchmark_suites::load(state.config().paths(), &normalized_suite_id)
            .map_err(internal_error)?
            .ok_or_else(benchmark_suite_not_found_error)?;
    if manifest.schema != "croopor.launch.benchmark.suite" || manifest.schema_version != 2 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "benchmark suite manifest is not current schema" })),
        ));
    }

    let proofs = family_c_qualification_proofs(state.config().paths(), &manifest)?;
    Ok(family_c_qualification_manifest_payload(
        &manifest,
        &proofs,
        [Vec::new(), Vec::new()],
    ))
}

fn family_c_qualification_preview_payload()
-> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let manifest = family_c_qualification_preview_manifest()?;
    let mut payload = family_c_qualification_manifest_payload(
        &manifest,
        &[],
        [
            vec!["suite_manifest_missing"],
            vec!["suite_manifest_missing", "managed_comparison_missing"],
        ],
    );
    payload["suite"] = json!({
        "present": false,
        "mode": FAMILY_C_QUALIFICATION_MODE,
        "run_count": manifest.runs.len(),
    });

    Ok(payload)
}

fn family_c_qualification_preview_manifest() -> Result<
    crate::state::benchmark_suites::BenchmarkSuiteManifest,
    (StatusCode, Json<serde_json::Value>),
> {
    let plan = matrix::benchmark_suite_plan(FAMILY_C_QUALIFICATION_MODE)
        .ok_or_else(unsupported_suite_mode_error)?;
    let runs = benchmark_suite_manifest_run_inputs(FAMILY_C_QUALIFICATION_MODE, &plan)
        .into_iter()
        .map(
            |run| crate::state::benchmark_suites::BenchmarkSuiteManifestRun {
                run_index: run.run_index,
                profile: run.profile,
                run_type: run.run_type,
                target_id: run.target_id.unwrap_or_default(),
                benchmark_id: run.benchmark_id,
                session_id: None,
                launched_at: None,
                state: "pending".to_string(),
            },
        )
        .collect();

    Ok(crate::state::benchmark_suites::BenchmarkSuiteManifest {
        schema: "croopor.launch.benchmark.suite".to_string(),
        schema_version: 2,
        suite_id: "family-c-1-12-2-preview".to_string(),
        instance_id: "preview".to_string(),
        mode: FAMILY_C_QUALIFICATION_MODE.to_string(),
        created_at: "preview".to_string(),
        updated_at: "preview".to_string(),
        runs,
    })
}

fn family_c_qualification_manifest_payload(
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    proofs: &[crate::state::launch_reports::LaunchProofRecord],
    extra_missing: [Vec<&'static str>; 2],
) -> serde_json::Value {
    let [baseline_target, managed_target] = family_c_qualification_targets();
    let [baseline_extra_missing, managed_extra_missing] = extra_missing;
    let baseline = family_c_qualification_target_payload(
        baseline_target,
        manifest,
        proofs,
        &baseline_extra_missing,
    );
    let managed = family_c_qualification_target_payload(
        managed_target,
        manifest,
        proofs,
        &managed_extra_missing,
    );
    let status = if family_c_qualification_target_ready(&baseline)
        && family_c_qualification_target_ready(&managed)
    {
        "ready"
    } else {
        "incomplete"
    };

    json!({
        "schema": FAMILY_C_QUALIFICATION_SCHEMA,
        "schema_version": FAMILY_C_QUALIFICATION_SCHEMA_VERSION,
        "status": status,
        "suite": {
            "suite_id": bounded_descriptor_token(&manifest.suite_id, "suite"),
            "mode": bounded_descriptor_token(&manifest.mode, "mode"),
            "run_count": manifest.runs.len(),
        },
        "target": {
            "family": "C",
            "loader": FAMILY_C_QUALIFICATION_LOADER,
            "version": FAMILY_C_QUALIFICATION_VERSION,
            "mode": FAMILY_C_QUALIFICATION_MODE,
        },
        "targets": [baseline, managed],
    })
}

fn family_c_qualification_proofs(
    paths: &croopor_config::AppPaths,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
) -> Result<
    Vec<crate::state::launch_reports::LaunchProofRecord>,
    (StatusCode, Json<serde_json::Value>),
> {
    let mut proofs =
        crate::state::launch_reports::list_recent(paths, FAMILY_C_QUALIFICATION_PROOF_SCAN_LIMIT)
            .map_err(internal_error)?;
    for session_id in manifest
        .runs
        .iter()
        .filter_map(|run| run.session_id.as_deref())
    {
        let already_loaded = proofs.iter().any(|proof| proof.session_id == session_id);
        if already_loaded {
            continue;
        }
        if let Some(proof) =
            crate::state::launch_reports::load(paths, session_id).map_err(internal_error)?
        {
            proofs.push(proof);
        }
    }

    Ok(proofs)
}

fn family_c_qualification_target_payload(
    target: FamilyCQualificationTarget,
    manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    proofs: &[crate::state::launch_reports::LaunchProofRecord],
    extra_missing: &[&'static str],
) -> serde_json::Value {
    let mut missing = Vec::new();
    missing.extend(extra_missing.iter().copied());
    let run = manifest
        .runs
        .iter()
        .find(|run| run.target_id.trim() == target.target_id);
    let proof = run.and_then(|run| family_c_qualification_matching_proof(run, proofs));

    if run.is_none() {
        missing.push("suite_run_missing");
    }
    if manifest.mode.trim() != FAMILY_C_QUALIFICATION_MODE {
        missing.push("suite_mode_mismatch");
    }

    if let Some(run) = run {
        if run.profile.trim() != target.profile {
            missing.push("suite_run_profile_mismatch");
        }
        if run.run_type.trim() != target.run_type {
            missing.push("suite_run_type_mismatch");
        }
        if run.benchmark_id.trim().is_empty() {
            missing.push("suite_run_benchmark_id_missing");
        } else if !run.benchmark_id.contains(target.target_id) {
            missing.push("suite_run_benchmark_id_target_mismatch");
        }
        if run.session_id.as_deref().and_then(trimmed_string).is_none() {
            missing.push("suite_run_session_missing");
        }
    }

    match proof {
        Some(proof) => {
            if proof.scenario.benchmark_id.as_deref() != run.map(|run| run.benchmark_id.as_str()) {
                missing.push("proof_benchmark_id_mismatch");
            }
            if !proof
                .scenario
                .benchmark_id
                .as_deref()
                .is_some_and(|benchmark_id| benchmark_id.contains(target.target_id))
            {
                missing.push("proof_benchmark_id_target_missing");
            }
            if proof.scenario.benchmark_profile.as_deref() != Some(target.profile) {
                missing.push("proof_profile_mismatch");
            }
            if proof.scenario.benchmark_run_type.as_deref() != Some(target.run_type) {
                missing.push("proof_run_type_mismatch");
            }
            if proof.scenario.benchmark_mode.as_deref() != Some(FAMILY_C_QUALIFICATION_MODE) {
                missing.push("proof_mode_mismatch");
            }
            if family_c_proof_version(proof) != Some(FAMILY_C_QUALIFICATION_VERSION) {
                missing.push("proof_version_mismatch");
            }
            if proof.scenario.performance_mode.trim() != target.performance_mode {
                missing.push("proof_performance_mode_mismatch");
            }
            if !family_c_qualification_outcome_is_acceptable(&proof.outcome) {
                missing.push("proof_outcome_not_comparable");
            }
            if target.comparison_required && proof.comparison.is_none() {
                missing.push("managed_comparison_missing");
            }
        }
        None => missing.push("proof_missing"),
    }

    missing.sort_unstable();
    missing.dedup();

    json!({
        "role": target.role,
        "target_id": target.target_id,
        "family": "C",
        "loader": FAMILY_C_QUALIFICATION_LOADER,
        "version": FAMILY_C_QUALIFICATION_VERSION,
        "required": {
            "profile": target.profile,
            "run_type": target.run_type,
            "mode": FAMILY_C_QUALIFICATION_MODE,
            "performance_mode": target.performance_mode,
        },
        "suite_run": family_c_qualification_suite_run_payload(run),
        "proof": family_c_qualification_proof_payload(proof),
        "missing": missing,
    })
}

fn family_c_qualification_matching_proof<'a>(
    run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
    proofs: &'a [crate::state::launch_reports::LaunchProofRecord],
) -> Option<&'a crate::state::launch_reports::LaunchProofRecord> {
    if let Some(session_id) = run.session_id.as_deref().and_then(trimmed_string) {
        if let Some(proof) = proofs.iter().find(|proof| proof.session_id == session_id) {
            return Some(proof);
        }
    }

    proofs.iter().find(|proof| {
        proof.scenario.benchmark_id.as_deref() == Some(run.benchmark_id.as_str())
            && proof
                .scenario
                .benchmark_id
                .as_deref()
                .is_some_and(|benchmark_id| benchmark_id.contains(run.target_id.as_str()))
    })
}

fn family_c_qualification_suite_run_payload(
    run: Option<&crate::state::benchmark_suites::BenchmarkSuiteManifestRun>,
) -> serde_json::Value {
    let Some(run) = run else {
        return json!({ "present": false });
    };

    json!({
        "present": true,
        "run_index": run.run_index,
        "profile": bounded_descriptor_token(&run.profile, "profile"),
        "run_type": bounded_descriptor_token(&run.run_type, "run-type"),
        "target_id": bounded_descriptor_token(&run.target_id, "target"),
        "benchmark_id": bounded_descriptor_token(&run.benchmark_id, "benchmark"),
        "session_id": run.session_id.as_deref().map(|value| bounded_descriptor_token(value, "session")),
        "state": bounded_descriptor_token(&run.state, "state"),
    })
}

fn family_c_qualification_proof_payload(
    proof: Option<&crate::state::launch_reports::LaunchProofRecord>,
) -> serde_json::Value {
    let Some(proof) = proof else {
        return json!({ "present": false });
    };
    let comparison = proof.comparison.as_ref().map(|comparison| {
        json!({
            "present": true,
            "baseline_session_id": bounded_descriptor_token(
                &comparison.baseline_session_id,
                "session"
            ),
            "metric_name": bounded_descriptor_token(&comparison.metric_name, "metric"),
            "matched_sample_count": comparison.matched_sample_count,
        })
    });

    json!({
        "present": true,
        "session_id": bounded_descriptor_token(&proof.session_id, "session"),
        "benchmark_id": proof
            .scenario
            .benchmark_id
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "benchmark")),
        "profile": proof
            .scenario
            .benchmark_profile
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "profile")),
        "run_type": proof
            .scenario
            .benchmark_run_type
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "run-type")),
        "mode": proof
            .scenario
            .benchmark_mode
            .as_deref()
            .map(|value| bounded_descriptor_token(value, "mode")),
        "performance_mode": bounded_descriptor_token(&proof.scenario.performance_mode, "mode"),
        "version": family_c_proof_version(proof)
            .map(|value| bounded_descriptor_token(value, "version")),
        "outcome": bounded_descriptor_token(&proof.outcome, "outcome"),
        "comparison": comparison.unwrap_or_else(|| json!({ "present": false })),
    })
}

fn family_c_qualification_target_ready(target: &serde_json::Value) -> bool {
    target
        .get("missing")
        .and_then(|missing| missing.as_array())
        .is_some_and(Vec::is_empty)
}

fn family_c_proof_version(proof: &crate::state::launch_reports::LaunchProofRecord) -> Option<&str> {
    proof
        .scenario
        .version_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "unknown")
        .or_else(|| {
            let value = proof.version_id.trim();
            (!value.is_empty() && value != "unknown").then_some(value)
        })
}

fn family_c_qualification_outcome_is_acceptable(outcome: &str) -> bool {
    matches!(outcome.trim(), "running" | "exited" | "completed")
}

fn bounded_descriptor_token(value: &str, fallback_prefix: &str) -> String {
    let value = value.trim();
    let safe = !value.is_empty()
        && value.len() <= 96
        && value
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'));
    if safe {
        return value.to_string();
    }

    format!("{fallback_prefix}-{:016x}", status_hash(value))
}

fn clamp_benchmark_suite_driver_interval_ms(value: Option<i64>) -> u64 {
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

fn is_terminal_benchmark_suite_driver_state(state: &str) -> bool {
    matches!(state, "complete" | "failed" | "stopped" | "interrupted")
}

fn benchmark_suite_not_found_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "benchmark suite not found" })),
    )
}

fn benchmark_suite_driver_not_found_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "benchmark suite driver not found" })),
    )
}

fn benchmark_suite_driver_already_active_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite driver is already active" })),
    )
}

fn benchmark_suite_complete_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite is complete" })),
    )
}

fn benchmark_suite_active_run_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "benchmark suite has active run" })),
    )
}

fn unsupported_suite_mode_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "suite_mode is not supported" })),
    )
}

async fn ensure_no_active_benchmark_suite_auto_run(
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

async fn benchmark_suite_driver_decision(
    sessions: &crate::state::SessionStore,
    input: BenchmarkSuitePlanInput,
) -> Result<BenchmarkSuiteDriverDecision, (StatusCode, Json<serde_json::Value>)> {
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

    if let Some(manifest) = input.manifest.as_ref() {
        if let Some(active_session_id) = active_benchmark_suite_session_id(sessions, manifest).await
        {
            return Ok(BenchmarkSuiteDriverDecision::Active {
                suite,
                active_session_id,
            });
        }
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

fn spawn_benchmark_suite_driver_loop(
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        run_benchmark_suite_driver_loop(state, driver_id, request, interval_ms, stop_rx).await;
    });
}

async fn run_benchmark_suite_driver_loop(
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *stop_rx.borrow() {
            state
                .benchmark_suite_drivers()
                .record_stopped(&driver_id)
                .await;
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
                    .await;
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
                    .await;
            }
            Ok(BenchmarkSuiteDriverDecision::Complete { .. }) => {
                state
                    .benchmark_suite_drivers()
                    .record_complete(&driver_id, summary)
                    .await;
                break;
            }
            Ok(BenchmarkSuiteDriverDecision::Launch(input)) => {
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
                            .await;
                    }
                    Err(error) => {
                        state
                            .benchmark_suite_drivers()
                            .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                            .await;
                        break;
                    }
                }
            }
            Err(error) => {
                state
                    .benchmark_suite_drivers()
                    .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                    .await;
                break;
            }
        }

        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    state
                        .benchmark_suite_drivers()
                        .record_stopped(&driver_id)
                        .await;
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
        }
    }
}

fn benchmark_suite_api_error_message(error: &(StatusCode, Json<serde_json::Value>)) -> String {
    error
        .1
        .0
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("benchmark suite driver failed")
        .to_string()
}

async fn active_benchmark_suite_session_id(
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

fn bounded_status_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let is_safe = value.len() <= 96
        && value
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'));
    if is_safe {
        return Some(value.to_string());
    }

    Some(format!("session-{:016x}", status_hash(value)))
}

fn status_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_launcher::{LaunchSessionRecord, LaunchStageRecord, LaunchState, SessionId};
    use croopor_performance::PerformanceManager;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt;

    #[test]
    fn benchmark_launch_request_missing_instance_id_returns_json_error() {
        let error = BenchmarkLaunchRequest {
            instance_id: None,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
            profile: Some("dev".to_string()),
            run_type: Some("repeat".to_string()),
            benchmark_mode: None,
            suite_mode: None,
            suite_id: None,
            run_index: None,
            interval_ms: None,
        }
        .into_launch_input()
        .expect_err("missing instance_id should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance_id is required" })
        );
    }

    #[test]
    fn benchmark_launch_request_rejects_old_benchmark_metadata_fields() {
        let error = serde_json::from_value::<BenchmarkLaunchRequest>(serde_json::json!({
            "instance_id": "instance",
            "benchmark_profile": "dev",
            "benchmark_run_type": "repeat"
        }))
        .expect_err("old benchmark metadata request fields should be rejected");

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn launch_success_response_payload_exposes_effective_memory() {
        let payload = launch_success_response_payload(&runner::LaunchSuccess {
            session_id: "session-1".to_string(),
            instance_id: "instance-1".to_string(),
            pid: 1234,
            launched_at: "2026-05-30T00:00:00Z".to_string(),
            max_memory_mb: 6144,
            min_memory_mb: 1024,
            healing: None,
            guardian: None,
        });

        assert_eq!(payload["status"], serde_json::json!("launching"));
        assert_eq!(payload["max_memory_mb"], serde_json::json!(6144));
        assert_eq!(payload["min_memory_mb"], serde_json::json!(1024));
    }

    #[test]
    fn benchmark_launch_request_rejects_suite_mode_field() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": " instance ",
            "suite_mode": "qual"
        }))
        .expect("deserialize benchmark launch request");

        let error = request
            .into_launch_input()
            .expect_err("suite_mode should not be accepted by benchmark launch");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "suite_mode is only supported for benchmark suite requests" })
        );
    }

    #[test]
    fn benchmark_suite_request_missing_instance_id_returns_json_error() {
        let error = BenchmarkLaunchRequest {
            instance_id: None,
            username: Some("Player".to_string()),
            max_memory_mb: Some(4096),
            min_memory_mb: Some(1024),
            client_started_at_ms: Some(123),
            profile: None,
            run_type: None,
            benchmark_mode: None,
            suite_mode: Some("development".to_string()),
            suite_id: None,
            run_index: Some(0),
            interval_ms: None,
        }
        .into_suite_launch_input()
        .expect_err("missing instance_id should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance_id is required" })
        );
    }

    #[test]
    fn benchmark_suite_defaults_to_development_first_run() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": " instance ",
            "username": "Player",
            "max_memory_mb": 4096,
            "min_memory_mb": 1024,
            "client_started_at_ms": 42
        }))
        .expect("deserialize suite request");

        let input = request
            .into_suite_launch_input()
            .expect("suite input should parse");

        assert_eq!(input.launch.instance_id, "instance");
        assert_eq!(input.launch.username.as_deref(), Some("Player"));
        assert_eq!(input.launch.max_memory_mb, Some(4096));
        assert_eq!(input.launch.min_memory_mb, Some(1024));
        assert_eq!(input.launch.client_started_at_ms, Some(42));
        assert_eq!(input.mode, "development");
        assert_eq!(input.run_index, 0);
        assert_eq!(input.plan.len(), 2);
        assert_eq!(input.plan[0].profile, "vanilla_baseline");
        assert_eq!(input.plan[0].run_type, "coldish");
    }

    #[test]
    fn benchmark_suite_omitted_run_index_resumes_first_unlaunched_manifest_run() {
        let root = test_root("suite-auto-resume");
        let paths = test_paths(&root);
        let suite_id = "suite-auto-resume";
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &manifest_runs,
            0,
            "session-0",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist launched run");
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": suite_id
        }))
        .expect("deserialize suite request");

        let input = request
            .into_suite_launch_input_with_manifest(Some(&paths))
            .expect("suite input should parse");

        assert_eq!(input.run_index, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_suite_omitted_run_index_conflicts_when_manifest_is_complete() {
        let root = test_root("suite-auto-complete");
        let paths = test_paths(&root);
        let suite_id = "suite-auto-complete";
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &manifest_runs,
            0,
            "session-0",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist first launched run");
        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &manifest_runs,
            1,
            "session-1",
            "2026-01-01T00:01:00.000Z",
        )
        .expect("persist second launched run");
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": suite_id
        }))
        .expect("deserialize suite request");

        let error = request
            .into_suite_launch_input_with_manifest(Some(&paths))
            .expect_err("complete suite should conflict");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite is complete" })
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_suite_omitted_run_index_without_manifest_selects_first_run() {
        let root = test_root("suite-auto-no-manifest");
        let paths = test_paths(&root);
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": "suite-auto-no-manifest"
        }))
        .expect("deserialize suite request");

        let input = request
            .into_suite_launch_input_with_manifest(Some(&paths))
            .expect("suite input should parse");

        assert_eq!(input.run_index, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_suite_explicit_run_index_bypasses_manifest_auto_selection() {
        let root = test_root("suite-explicit-bypass");
        let paths = test_paths(&root);
        let suite_id = "suite-explicit-bypass";
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &manifest_runs,
            0,
            "session-0",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist launched run");
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": suite_id,
            "run_index": 0
        }))
        .expect("deserialize suite request");

        let input = request
            .into_suite_launch_input_with_manifest(Some(&paths))
            .expect("suite input should parse");

        assert_eq!(input.run_index, 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_suite_request_accepts_current_suite_mode_ids() {
        let suite_mode_request: BenchmarkLaunchRequest = serde_json::from_value(
            serde_json::json!({ "instance_id": "instance", "suite_mode": "release_validation" }),
        )
        .expect("deserialize suite_mode request");

        assert_eq!(
            suite_mode_request
                .into_suite_launch_input()
                .expect("suite mode input")
                .mode,
            "release_validation"
        );
    }

    #[test]
    fn benchmark_suite_request_rejects_suite_mode_aliases() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "qual"
        }))
        .expect("deserialize suite request");

        let error = request
            .into_suite_launch_input()
            .expect_err("suite mode alias should not be accepted");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "suite_mode is not supported" })
        );
    }

    #[test]
    fn benchmark_suite_request_rejects_benchmark_mode_field() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "benchmark_mode": "release_validation"
        }))
        .expect("deserialize suite request");

        let error = request
            .into_suite_launch_input()
            .expect_err("benchmark_mode should not be accepted by benchmark suite requests");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark_mode is only supported for benchmark launch requests" })
        );
    }

    #[test]
    fn benchmark_suite_request_rejects_out_of_range_run_index() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "run_index": 2
        }))
        .expect("deserialize suite request");

        let error = request
            .into_suite_launch_input()
            .expect_err("run_index outside development plan should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "run_index is out of range" })
        );
    }

    #[test]
    fn benchmark_suite_request_rejects_negative_run_index() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "run_index": -1
        }))
        .expect("deserialize suite request");

        let error = request
            .into_suite_launch_input()
            .expect_err("negative run_index should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "run_index is out of range" })
        );
    }

    #[test]
    fn benchmark_suite_response_payload_exposes_selected_and_remaining_runs() {
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let payload = benchmark_suite_status_payload("suite-dev", "development", 0, &plan);

        assert_eq!(
            payload,
            serde_json::json!({
                "suite_id": "suite-dev",
                "mode": "development",
                "run_index": 0,
                "run_count": 2,
                "selected_profile": "vanilla_baseline",
                "selected_run_type": "coldish",
                "selected_target_id": null,
                "selected": {
                    "run_index": 0,
                    "profile": "vanilla_baseline",
                    "run_type": "coldish",
                    "target_id": null,
                    "benchmark_id": "suite-development-00-vanilla_baseline-coldish",
                },
                "remaining": [
                    {
                        "run_index": 1,
                        "profile": "managed_default",
                        "run_type": "repeat",
                        "target_id": null,
                        "benchmark_id": "suite-development-01-managed_default-repeat",
                    }
                ],
            })
        );
    }

    #[test]
    fn benchmark_suite_release_validation_carries_family_c_target_identity() {
        let plan = matrix::benchmark_suite_plan("release_validation").expect("release plan");
        let payload =
            benchmark_suite_status_payload("suite-release", "release_validation", 0, &plan);
        let manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);

        assert_eq!(
            payload["selected_target_id"],
            serde_json::json!("family_c_forge_1_12_2_vanilla_baseline")
        );
        assert_eq!(
            payload["selected"]["target_id"],
            serde_json::json!("family_c_forge_1_12_2_vanilla_baseline")
        );
        assert_eq!(
            payload["remaining"][0]["target_id"],
            serde_json::json!("family_c_forge_1_12_2_family_c_forge_core")
        );
        assert_eq!(
            manifest_runs[0].target_id.as_deref(),
            Some("family_c_forge_1_12_2_vanilla_baseline")
        );
        assert_eq!(
            manifest_runs[1].target_id.as_deref(),
            Some("family_c_forge_1_12_2_family_c_forge_core")
        );
    }

    #[test]
    fn benchmark_suite_manifest_persists_family_c_target_identity() {
        let root = test_root("suite-family-c-target-manifest");
        let paths = test_paths(&root);
        let suite_id = "suite-family-c-target-manifest";
        let plan = matrix::benchmark_suite_plan("release_validation").expect("release plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);

        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "release_validation",
            &manifest_runs,
            1,
            "session-1",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist launched run");
        let manifest = crate::state::benchmark_suites::load(&paths, suite_id)
            .expect("load suite")
            .expect("suite should exist");

        assert_eq!(manifest.schema_version, 2);
        assert_eq!(
            manifest.runs[0].target_id,
            "family_c_forge_1_12_2_vanilla_baseline"
        );
        assert_eq!(
            manifest.runs[1].target_id,
            "family_c_forge_1_12_2_family_c_forge_core"
        );
        assert!(manifest.runs.len() <= 16);

        cleanup(&root);
    }

    #[test]
    fn benchmark_suite_ids_include_family_c_target_identity_and_stay_bounded() {
        let plan = matrix::benchmark_suite_plan("release_validation").expect("release plan");
        let baseline_id = benchmark_suite_run_id("release_validation", 0, plan[0]);
        let managed_id = benchmark_suite_run_id("release_validation", 1, plan[1]);

        assert_ne!(baseline_id, managed_id);
        assert_eq!(
            baseline_id,
            "suite-release_validation-00-family_c_forge_1_12_2_vanilla_baseline-coldish"
        );
        assert_eq!(
            managed_id,
            "suite-release_validation-01-family_c_forge_1_12_2_family_c_forge_core-coldish"
        );
        for benchmark_id in [baseline_id, managed_id] {
            assert!(benchmark_id.len() <= 96);
            assert!(
                benchmark_id
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
            );
        }
    }

    #[tokio::test]
    async fn family_c_qualification_complete_suite_and_proofs_are_ready() {
        let fixture = RouteTestFixture::new("family-c-qualification-ready");
        let suite_id = "family-c-qualification-ready";
        persist_family_c_suite_run(&fixture.paths, suite_id, 0, "baseline-session");
        persist_family_c_suite_run(&fixture.paths, suite_id, 1, "managed-session");
        let manifest = crate::state::benchmark_suites::load(&fixture.paths, suite_id)
            .expect("load suite")
            .expect("suite exists");
        let baseline_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
            .expect("baseline run");
        let managed_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_MANAGED_TARGET_ID)
            .expect("managed run");
        write_family_c_proof(&fixture.paths, baseline_run, "vanilla", None);
        write_family_c_proof(
            &fixture.paths,
            managed_run,
            "managed",
            Some(crate::state::launch_reports::LaunchProofComparison {
                baseline_session_id: "baseline-session".to_string(),
                baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
                matched_sample_count: 1,
                metric_name: "total_completed_stage_duration_ms".to_string(),
                current_value_ms: 90,
                baseline_value_ms: 120,
                delta_ms: -30,
                delta_percent: -25.0,
            }),
        );

        let payload = family_c_qualification_payload(&fixture.state, suite_id)
            .await
            .expect("qualification payload");

        assert_eq!(payload["status"], serde_json::json!("ready"));
        assert_eq!(
            payload["target"],
            serde_json::json!({
                "family": "C",
                "loader": "Forge",
                "version": "1.12.2",
                "mode": "release_validation",
            })
        );
        assert_eq!(
            payload["targets"][0]["target_id"],
            serde_json::json!(FAMILY_C_BASELINE_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][1]["target_id"],
            serde_json::json!(FAMILY_C_MANAGED_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][0]["proof"]["present"],
            serde_json::json!(true)
        );
        assert_eq!(
            payload["targets"][1]["proof"]["present"],
            serde_json::json!(true)
        );
        assert!(
            payload["targets"][0]["proof"]["benchmark_id"]
                .as_str()
                .expect("baseline proof benchmark id")
                .contains(FAMILY_C_BASELINE_TARGET_ID)
        );
        assert!(
            payload["targets"][1]["proof"]["benchmark_id"]
                .as_str()
                .expect("managed proof benchmark id")
                .contains(FAMILY_C_MANAGED_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][1]["proof"]["comparison"]["present"],
            serde_json::json!(true)
        );
        assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
        assert_eq!(payload["targets"][1]["missing"], serde_json::json!([]));

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn family_c_qualification_missing_baseline_and_managed_evidence_is_incomplete() {
        let fixture = RouteTestFixture::new("family-c-qualification-incomplete");
        let suite_id = "family-c-qualification-incomplete";
        persist_family_c_suite_run(&fixture.paths, suite_id, 2, "legacy-session");

        let payload = family_c_qualification_payload(&fixture.state, suite_id)
            .await
            .expect("qualification payload");

        assert_eq!(payload["status"], serde_json::json!("incomplete"));
        assert_eq!(
            payload["targets"][0]["missing"],
            serde_json::json!(["proof_missing", "suite_run_session_missing"])
        );
        assert_eq!(
            payload["targets"][1]["missing"],
            serde_json::json!(["proof_missing", "suite_run_session_missing"])
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn family_c_qualification_preview_route_is_incomplete_without_suite_id() {
        let fixture = RouteTestFixture::new("family-c-qualification-preview-route");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/launch/benchmark/qualification/family-c-1-12-2/preview")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("preview json");
        assert_eq!(payload["status"], serde_json::json!("incomplete"));
        assert_eq!(payload["suite"]["present"], serde_json::json!(false));
        assert_eq!(
            payload["targets"][0]["target_id"],
            serde_json::json!(FAMILY_C_BASELINE_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][1]["target_id"],
            serde_json::json!(FAMILY_C_MANAGED_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][0]["missing"],
            serde_json::json!([
                "proof_missing",
                "suite_manifest_missing",
                "suite_run_session_missing"
            ])
        );
        assert_eq!(
            payload["targets"][1]["missing"],
            serde_json::json!([
                "managed_comparison_missing",
                "proof_missing",
                "suite_manifest_missing",
                "suite_run_session_missing"
            ])
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn family_c_qualification_route_returns_ready_for_complete_suite() {
        let fixture = RouteTestFixture::new("family-c-qualification-ready-route");
        let suite_id = "family-c-qualification-ready-route";
        persist_family_c_suite_run(&fixture.paths, suite_id, 0, "baseline-session");
        persist_family_c_suite_run(&fixture.paths, suite_id, 1, "managed-session");
        let manifest = crate::state::benchmark_suites::load(&fixture.paths, suite_id)
            .expect("load suite")
            .expect("suite exists");
        let baseline_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
            .expect("baseline run");
        let managed_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_MANAGED_TARGET_ID)
            .expect("managed run");
        write_family_c_proof(&fixture.paths, baseline_run, "vanilla", None);
        write_family_c_proof(
            &fixture.paths,
            managed_run,
            "managed",
            Some(crate::state::launch_reports::LaunchProofComparison {
                baseline_session_id: "baseline-session".to_string(),
                baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
                matched_sample_count: 1,
                metric_name: "total_completed_stage_duration_ms".to_string(),
                current_value_ms: 90,
                baseline_value_ms: 120,
                delta_ms: -30,
                delta_percent: -25.0,
            }),
        );

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/api/v1/launch/benchmark/qualification/family-c-1-12-2/{suite_id}"
                    ))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("qualification json");
        let data = serde_json::to_string(&payload).expect("serialize payload");
        let lower_data = data.to_ascii_lowercase();

        assert_eq!(payload["status"], serde_json::json!("ready"));
        assert_eq!(
            payload["targets"][0]["target_id"],
            serde_json::json!(FAMILY_C_BASELINE_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][1]["target_id"],
            serde_json::json!(FAMILY_C_MANAGED_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][0]["proof"]["present"],
            serde_json::json!(true)
        );
        assert_eq!(
            payload["targets"][1]["proof"]["present"],
            serde_json::json!(true)
        );
        assert_eq!(
            payload["targets"][1]["proof"]["comparison"]["present"],
            serde_json::json!(true)
        );
        assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
        assert_eq!(payload["targets"][1]["missing"], serde_json::json!([]));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("java-args"));
        assert!(!lower_data.contains("account"));
        assert!(!lower_data.contains("token"));

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn family_c_qualification_route_missing_suite_returns_json_404() {
        let fixture = RouteTestFixture::new("family-c-qualification-missing-route");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/launch/benchmark/qualification/family-c-1-12-2/missing-suite")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("qualification error json");
        assert_eq!(
            payload,
            serde_json::json!({ "error": "benchmark suite not found" })
        );

        cleanup(&fixture.root);
    }

    #[test]
    fn family_c_qualification_preview_payload_is_descriptor_only() {
        let payload =
            family_c_qualification_preview_payload().expect("family c qualification preview");
        let data = serde_json::to_string(&payload).expect("serialize payload");
        let lower_data = data.to_ascii_lowercase();

        assert_eq!(payload["status"], serde_json::json!("incomplete"));
        assert_eq!(
            payload["target"],
            serde_json::json!({
                "family": "C",
                "loader": "Forge",
                "version": "1.12.2",
                "mode": "release_validation",
            })
        );
        assert_eq!(
            payload["targets"][0]["target_id"],
            serde_json::json!(FAMILY_C_BASELINE_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][1]["target_id"],
            serde_json::json!(FAMILY_C_MANAGED_TARGET_ID)
        );
        assert_eq!(
            payload["targets"][0]["suite_run"]["benchmark_id"],
            serde_json::json!(
                "suite-release_validation-00-family_c_forge_1_12_2_vanilla_baseline-coldish"
            )
        );
        assert_eq!(
            payload["targets"][1]["suite_run"]["benchmark_id"],
            serde_json::json!(
                "suite-release_validation-01-family_c_forge_1_12_2_family_c_forge_core-coldish"
            )
        );

        assert!(data.len() < 4096);
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("java-args"));
        assert!(!lower_data.contains("account"));
        assert!(!lower_data.contains("token"));
        assert!(!lower_data.contains("runtime"));
    }

    #[tokio::test]
    async fn family_c_qualification_wrong_suite_mode_is_incomplete() {
        let fixture = RouteTestFixture::new("family-c-qualification-wrong-mode");
        let suite_id = "family-c-qualification-wrong-mode";
        persist_family_c_suite_run(&fixture.paths, suite_id, 0, "baseline-session");
        persist_family_c_suite_run(&fixture.paths, suite_id, 1, "managed-session");
        let mut manifest = crate::state::benchmark_suites::load(&fixture.paths, suite_id)
            .expect("load suite")
            .expect("suite exists");
        let baseline_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
            .expect("baseline run");
        let managed_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_MANAGED_TARGET_ID)
            .expect("managed run");
        write_family_c_proof(&fixture.paths, baseline_run, "vanilla", None);
        write_family_c_proof(
            &fixture.paths,
            managed_run,
            "managed",
            Some(crate::state::launch_reports::LaunchProofComparison {
                baseline_session_id: "baseline-session".to_string(),
                baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
                matched_sample_count: 1,
                metric_name: "total_completed_stage_duration_ms".to_string(),
                current_value_ms: 90,
                baseline_value_ms: 120,
                delta_ms: -30,
                delta_percent: -25.0,
            }),
        );
        manifest.mode = "development".to_string();
        write_family_c_suite_manifest(&fixture.paths, &manifest);

        let payload = family_c_qualification_payload(&fixture.state, suite_id)
            .await
            .expect("qualification payload");

        assert_eq!(payload["status"], serde_json::json!("incomplete"));
        assert_eq!(payload["suite"]["mode"], serde_json::json!("development"));
        assert_eq!(
            payload["targets"][0]["missing"],
            serde_json::json!(["suite_mode_mismatch"])
        );
        assert_eq!(
            payload["targets"][1]["missing"],
            serde_json::json!(["suite_mode_mismatch"])
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn family_c_qualification_payload_excludes_sensitive_fields() {
        let fixture = RouteTestFixture::new("family-c-qualification-sensitive");
        let suite_id = "family-c-qualification-sensitive";
        persist_family_c_suite_run(&fixture.paths, suite_id, 0, "baseline-session");
        persist_family_c_suite_run(&fixture.paths, suite_id, 1, "managed-session");
        let manifest = crate::state::benchmark_suites::load(&fixture.paths, suite_id)
            .expect("load suite")
            .expect("suite exists");
        let baseline_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
            .expect("baseline run");
        let managed_run = manifest
            .runs
            .iter()
            .find(|run| run.target_id == FAMILY_C_MANAGED_TARGET_ID)
            .expect("managed run");
        write_family_c_proof(&fixture.paths, baseline_run, "vanilla", None);
        let mut managed_proof = family_c_proof_record(
            managed_run,
            "managed",
            Some(crate::state::launch_reports::LaunchProofComparison {
                baseline_session_id: "baseline-session".to_string(),
                baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
                matched_sample_count: 1,
                metric_name: "total_completed_stage_duration_ms".to_string(),
                current_value_ms: 90,
                baseline_value_ms: 120,
                delta_ms: -30,
                delta_percent: -25.0,
            }),
        );
        managed_proof.failure_detail =
            Some("C:\\Users\\SecretUser\\token --java-args --runtime-arguments".to_string());
        write_family_c_proof_record(&fixture.paths, &managed_proof);

        let payload = family_c_qualification_payload(&fixture.state, suite_id)
            .await
            .expect("qualification payload");
        let data = serde_json::to_string(&payload).expect("serialize payload");
        let lower_data = data.to_ascii_lowercase();

        assert!(data.len() < 4096);
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!data.contains("SecretUser"));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("java-args"));
        assert!(!lower_data.contains("account"));
        assert!(!lower_data.contains("token"));
        assert!(!lower_data.contains("runtime-arguments"));

        cleanup(&fixture.root);
    }

    #[test]
    fn benchmark_suite_request_accepts_or_derives_suite_id() {
        let explicit_request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": "../chosen suite"
        }))
        .expect("deserialize explicit suite id request");
        let derived_request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development"
        }))
        .expect("deserialize derived suite id request");

        let explicit = explicit_request
            .into_suite_launch_input()
            .expect("explicit suite id input");
        let derived = derived_request
            .into_suite_launch_input()
            .expect("derived suite id input");

        assert!(explicit.suite_id.starts_with("chosen_suite-"));
        assert!(derived.suite_id.starts_with("suite-instance-development-"));
        assert_eq!(
            derived.suite_id,
            crate::state::benchmark_suites::derive_suite_id("instance", "development")
        );
    }

    #[tokio::test]
    async fn benchmark_suite_driver_start_status_normalizes_mode_and_clamps_interval() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "release_validation",
            "suite_id": "../driver suite",
            "interval_ms": 1
        }))
        .expect("deserialize driver request");
        let input = request
            .into_suite_plan_input_with_manifest(None)
            .expect("driver plan input");
        let summary = benchmark_suite_driver_suite_summary(&input);
        let interval_ms = clamp_benchmark_suite_driver_interval_ms(Some(1));
        let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();

        let started = store
            .start(
                input.suite_id.clone(),
                input.mode.clone(),
                interval_ms,
                summary,
            )
            .await
            .expect("driver should start");
        let payload = benchmark_suite_driver_response_payload("scheduled", &started.status);

        assert_eq!(
            started.status.interval_ms,
            MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS
        );
        assert_eq!(payload["status"], serde_json::json!("scheduled"));
        assert_eq!(payload["driver"]["state"], serde_json::json!("scheduled"));
        assert_eq!(
            payload["driver"]["mode"],
            serde_json::json!("release_validation")
        );
        assert_eq!(
            payload["suite"]["mode"],
            serde_json::json!("release_validation")
        );
        assert!(
            payload["driver"]["suite_id"]
                .as_str()
                .expect("suite id")
                .starts_with("driver_suite-")
        );
    }

    #[tokio::test]
    async fn benchmark_suite_driver_duplicate_start_conflicts_until_terminal() {
        let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
        let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let first = store
            .start(
                "suite-driver".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("first driver starts");

        let conflict = store
            .start(
                "suite-driver".to_string(),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await;

        assert!(conflict.is_err());
        store.record_stopped(&first.status.id).await;
        store
            .start(
                "suite-driver".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("terminal driver no longer conflicts");
    }

    #[tokio::test]
    async fn benchmark_suite_driver_stop_reports_stopped_without_killing_sessions() {
        let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
        let sessions = crate::state::SessionStore::new();
        sessions.insert(test_record("active-suite-session")).await;
        let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 1,
            pending_run_index: Some(1),
        };
        let started = store
            .start(
                "suite-driver".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("driver starts");

        let stopped = store.stop(&started.status.id).await.expect("stop driver");

        assert_eq!(stopped.state, "stopped");
        let record = sessions
            .get("active-suite-session")
            .await
            .expect("session should remain");
        assert_eq!(record.state, LaunchState::Queued);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_list_payload_is_bounded_and_recent_first() {
        let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
        let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        for index in 0..30 {
            let started = store
                .start(
                    format!("suite-driver-{index}"),
                    "development".to_string(),
                    30_000,
                    summary.clone(),
                )
                .await
                .expect("driver starts");
            store.record_stopped(&started.status.id).await;
        }

        let drivers = store.list_recent(MAX_BENCHMARK_SUITE_DRIVER_LIST).await;
        let payload = benchmark_suite_driver_list_response_payload(&drivers);

        assert_eq!(drivers.len(), MAX_BENCHMARK_SUITE_DRIVER_LIST);
        assert_eq!(payload["status"], serde_json::json!("ok"));
        assert_eq!(
            payload["drivers"].as_array().expect("drivers array").len(),
            MAX_BENCHMARK_SUITE_DRIVER_LIST
        );
        assert_eq!(
            payload["drivers"][0]["driver"]["id"],
            serde_json::json!("benchmark-suite-driver-000000000000001e")
        );
        assert_eq!(
            payload["drivers"][0]["driver"]["state"],
            serde_json::json!("stopped")
        );
    }

    #[test]
    fn benchmark_suite_driver_unknown_status_error_uses_json_404() {
        let error = benchmark_suite_driver_not_found_error();

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite driver not found" })
        );
    }

    #[tokio::test]
    async fn benchmark_suite_driver_resume_missing_id_returns_404() {
        let fixture = RouteTestFixture::new("driver-resume-missing");

        let error = resume_benchmark_suite_driver(
            fixture.state.clone(),
            "benchmark-suite-driver-0000000000000001".to_string(),
        )
        .await
        .expect_err("missing driver should 404");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite driver not found" })
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_resume_rejects_non_terminal_driver() {
        let fixture = RouteTestFixture::new("driver-resume-active");
        let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let started = fixture
            .state
            .benchmark_suite_drivers()
            .start(
                "suite-resume-active".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("driver starts");

        let error = resume_benchmark_suite_driver(fixture.state.clone(), started.status.id)
            .await
            .expect_err("non-terminal driver should conflict");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite driver is already active" })
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_resume_missing_manifest_returns_404() {
        let fixture = RouteTestFixture::new("driver-resume-missing-manifest");
        let stopped = fixture
            .stopped_driver("suite-resume-missing-manifest", "development", 30_000)
            .await;

        let error = resume_benchmark_suite_driver(fixture.state.clone(), stopped.id)
            .await
            .expect_err("missing suite manifest should 404");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite not found" })
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_resume_complete_manifest_conflicts() {
        let fixture = RouteTestFixture::new("driver-resume-complete-manifest");
        let suite_id = "suite-resume-complete-manifest";
        fixture.persist_suite_runs(suite_id, &[0, 1]);
        let stopped = fixture
            .stopped_driver(suite_id, "development", 30_000)
            .await;

        let error = resume_benchmark_suite_driver(fixture.state.clone(), stopped.id)
            .await
            .expect_err("complete suite manifest should conflict");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite is complete" })
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_resume_starts_fresh_driver_from_terminal_record() {
        let fixture = RouteTestFixture::new("driver-resume-success");
        let suite_id = "suite-resume-success";
        fixture.persist_suite_runs(suite_id, &[0]);
        fixture
            .state
            .sessions()
            .insert(test_record("session-0"))
            .await;
        let stopped = fixture
            .stopped_driver(suite_id, "development", 45_000)
            .await;

        let payload = resume_benchmark_suite_driver(fixture.state.clone(), stopped.id.clone())
            .await
            .expect("terminal driver should resume");
        let resumed_id = payload["driver"]["id"].as_str().expect("new driver id");

        assert_eq!(payload["status"], serde_json::json!("scheduled"));
        assert_eq!(payload["resumed_from"], serde_json::json!(stopped.id));
        assert_eq!(resumed_id, "benchmark-suite-driver-0000000000000002");
        assert_ne!(resumed_id, stopped.id);
        assert_eq!(payload["driver"]["interval_ms"], serde_json::json!(45_000));
        assert_eq!(
            fixture
                .state
                .benchmark_suite_drivers()
                .get(&stopped.id)
                .await
                .expect("stopped driver remains visible")
                .state,
            "stopped"
        );
        fixture
            .state
            .benchmark_suite_drivers()
            .stop(resumed_id)
            .await
            .expect("stop resumed driver");
        tokio::time::sleep(Duration::from_millis(10)).await;

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_startup_resume_starts_fresh_driver_from_restart_interruption() {
        let fixture = RouteTestFixture::new("driver-auto-resume-success");
        let suite_id = "suite-auto-resume-success";
        fixture.persist_suite_runs(suite_id, &[0]);
        let interrupted = fixture.active_driver(suite_id, "development", 45_000).await;
        let reloaded = fixture.reload();
        reloaded
            .state
            .sessions()
            .insert(test_record("session-0"))
            .await;

        let summary =
            resume_restart_interrupted_benchmark_suite_drivers(reloaded.state.clone()).await;

        assert_eq!(
            summary,
            BenchmarkSuiteDriverResumeSummary {
                pending: 1,
                resumed: 1,
                failed: 0,
            }
        );
        let original = reloaded
            .state
            .benchmark_suite_drivers()
            .get(&interrupted.id)
            .await
            .expect("interrupted driver remains visible");
        assert_eq!(original.state, "interrupted");
        assert_eq!(
            original.error.as_deref(),
            Some("driver automatic resume started after restart")
        );
        let drivers = reloaded
            .state
            .benchmark_suite_drivers()
            .list_recent(5)
            .await;
        let fresh = drivers
            .iter()
            .find(|driver| driver.id != interrupted.id && driver.suite_id == suite_id)
            .expect("fresh resumed driver should be visible");
        assert_eq!(fresh.interval_ms, 45_000);
        assert!(matches!(
            fresh.state.as_str(),
            "scheduled" | "active" | "launched_next"
        ));
        reloaded
            .state
            .benchmark_suite_drivers()
            .stop(&fresh.id)
            .await
            .expect("stop fresh driver");
        tokio::time::sleep(Duration::from_millis(10)).await;

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_startup_resume_missing_manifest_fails_boundedly() {
        let fixture = RouteTestFixture::new("driver-auto-resume-missing-manifest");
        let interrupted = fixture
            .active_driver("suite-auto-resume-missing-manifest", "development", 30_000)
            .await;
        let reloaded = fixture.reload();

        let summary =
            resume_restart_interrupted_benchmark_suite_drivers(reloaded.state.clone()).await;

        assert_eq!(
            summary,
            BenchmarkSuiteDriverResumeSummary {
                pending: 1,
                resumed: 0,
                failed: 1,
            }
        );
        let original = reloaded
            .state
            .benchmark_suite_drivers()
            .get(&interrupted.id)
            .await
            .expect("interrupted driver remains visible");
        assert_eq!(original.state, "interrupted");
        assert_eq!(
            original.error.as_deref(),
            Some("driver automatic resume failed: benchmark suite not found")
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_startup_resume_complete_manifest_fails_boundedly() {
        let fixture = RouteTestFixture::new("driver-auto-resume-complete-manifest");
        let suite_id = "suite-auto-resume-complete-manifest";
        fixture.persist_suite_runs(suite_id, &[0, 1]);
        let interrupted = fixture.active_driver(suite_id, "development", 30_000).await;
        let reloaded = fixture.reload();

        let summary =
            resume_restart_interrupted_benchmark_suite_drivers(reloaded.state.clone()).await;

        assert_eq!(
            summary,
            BenchmarkSuiteDriverResumeSummary {
                pending: 1,
                resumed: 0,
                failed: 1,
            }
        );
        let original = reloaded
            .state
            .benchmark_suite_drivers()
            .get(&interrupted.id)
            .await
            .expect("interrupted driver remains visible");
        assert_eq!(original.state, "interrupted");
        assert_eq!(
            original.error.as_deref(),
            Some("driver automatic resume failed: benchmark suite is complete")
        );

        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn benchmark_suite_driver_error_status_payload_is_bounded_and_sanitized() {
        let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
        let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
            run_count: 2,
            launched_run_count: 0,
            pending_run_index: Some(0),
        };
        let started = store
            .start(
                "suite-sensitive".to_string(),
                "development".to_string(),
                30_000,
                summary,
            )
            .await
            .expect("driver starts");
        store
            .record_failed(
                &started.status.id,
                "failed command java_path /home/Secret/.minecraft --jvm-args username Secret",
            )
            .await;
        let status = store.get(&started.status.id).await.expect("driver status");
        let payload = benchmark_suite_driver_response_payload(&status.state, &status);
        let data = serde_json::to_string(&payload).expect("serialize driver payload");
        let lower_data = data.to_ascii_lowercase();

        assert!(data.len() < 2048);
        assert!(!data.contains("SecretUser"));
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("jvm"));
        assert!(!lower_data.contains("username"));
        assert!(!lower_data.contains("filesystem"));
        assert!(!lower_data.contains("args"));
    }

    #[test]
    fn benchmark_suite_driver_interval_uses_safe_bounds() {
        assert_eq!(
            clamp_benchmark_suite_driver_interval_ms(None),
            DEFAULT_BENCHMARK_SUITE_DRIVER_INTERVAL_MS
        );
        assert_eq!(
            clamp_benchmark_suite_driver_interval_ms(Some(-1)),
            MIN_BENCHMARK_SUITE_DRIVER_INTERVAL_MS
        );
        assert_eq!(
            clamp_benchmark_suite_driver_interval_ms(Some(60_000)),
            60_000
        );
        assert_eq!(
            clamp_benchmark_suite_driver_interval_ms(Some(9_999_999)),
            MAX_BENCHMARK_SUITE_DRIVER_INTERVAL_MS
        );
    }

    #[test]
    fn benchmark_suite_missing_lookup_error_uses_json_404() {
        let error = benchmark_suite_not_found_error();

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite not found" })
        );
    }

    #[tokio::test]
    async fn benchmark_suite_auto_driver_conflicts_when_manifest_run_is_active() {
        let store = crate::state::SessionStore::new();
        store.insert(test_record("active-suite-session")).await;
        let manifest = test_manifest(vec![
            test_manifest_run(0, Some("missing-session")),
            test_manifest_run(1, Some("active-suite-session")),
        ]);

        let error = ensure_no_active_benchmark_suite_auto_run(&store, Some(&manifest), true)
            .await
            .expect_err("active suite session should conflict");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "benchmark suite has active run" })
        );
    }

    #[tokio::test]
    async fn benchmark_suite_auto_driver_ignores_missing_terminal_and_explicit_runs() {
        let store = crate::state::SessionStore::new();
        let mut failed = test_record("failed-suite-session");
        failed.state = LaunchState::Failed;
        store.insert(failed).await;
        let mut exited = test_record("exited-suite-session");
        exited.state = LaunchState::Exited;
        store.insert(exited).await;
        store.insert(test_record("active-suite-session")).await;
        let terminal_manifest = test_manifest(vec![
            test_manifest_run(0, Some("missing-session")),
            test_manifest_run(1, Some("failed-suite-session")),
            test_manifest_run(2, Some("exited-suite-session")),
        ]);
        let active_manifest =
            test_manifest(vec![test_manifest_run(0, Some("active-suite-session"))]);

        ensure_no_active_benchmark_suite_auto_run(&store, None, true)
            .await
            .expect("missing manifest should not block");
        ensure_no_active_benchmark_suite_auto_run(&store, Some(&terminal_manifest), true)
            .await
            .expect("missing and terminal sessions should not block");
        ensure_no_active_benchmark_suite_auto_run(&store, Some(&active_manifest), false)
            .await
            .expect("explicit run_index path should bypass active auto-driver guard");
    }

    #[tokio::test]
    async fn benchmark_suite_tick_active_returns_non_error_without_launching() {
        let store = crate::state::SessionStore::new();
        store.insert(test_record("active-suite-session")).await;
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let input = BenchmarkSuitePlanInput {
            launch: task::LaunchRequest {
                instance_id: "instance".to_string(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            suite_id: "suite-test".to_string(),
            mode: "development".to_string(),
            plan,
            manifest: Some(test_manifest(vec![test_manifest_run(
                0,
                Some("active-suite-session"),
            )])),
        };

        let decision = benchmark_suite_driver_decision(&store, input)
            .await
            .expect("tick decision should succeed");

        match decision {
            BenchmarkSuiteDriverDecision::Active {
                suite,
                active_session_id,
            } => {
                assert_eq!(active_session_id, "active-suite-session");
                assert_eq!(suite["suite_id"], "suite-test");
                assert_eq!(suite["pending_run_index"], serde_json::json!(1));
            }
            BenchmarkSuiteDriverDecision::Complete { .. }
            | BenchmarkSuiteDriverDecision::Launch(_) => {
                panic!("active manifest run should not launch or complete")
            }
        }
    }

    #[tokio::test]
    async fn benchmark_suite_tick_complete_returns_non_error_without_launching() {
        let store = crate::state::SessionStore::new();
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let input = BenchmarkSuitePlanInput {
            launch: task::LaunchRequest {
                instance_id: "instance".to_string(),
                username: None,
                max_memory_mb: None,
                min_memory_mb: None,
                client_started_at_ms: None,
            },
            suite_id: "suite-test".to_string(),
            mode: "development".to_string(),
            plan,
            manifest: Some(test_manifest(vec![
                test_manifest_run(0, Some("session-0")),
                test_manifest_run(1, Some("session-1")),
            ])),
        };

        let decision = benchmark_suite_driver_decision(&store, input)
            .await
            .expect("tick decision should succeed");

        match decision {
            BenchmarkSuiteDriverDecision::Complete { suite } => {
                assert_eq!(suite["suite_id"], "suite-test");
                assert_eq!(suite["pending_run_index"], serde_json::Value::Null);
                assert_eq!(suite["launched_run_count"], serde_json::json!(2));
            }
            BenchmarkSuiteDriverDecision::Active { .. }
            | BenchmarkSuiteDriverDecision::Launch(_) => {
                panic!("complete manifest should not launch or report active")
            }
        }
    }

    #[tokio::test]
    async fn benchmark_suite_tick_selects_next_unlaunched_manifest_run() {
        let store = crate::state::SessionStore::new();
        let root = test_root("suite-tick-pending");
        let paths = test_paths(&root);
        let suite_id = "suite-tick-pending";
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        crate::state::benchmark_suites::persist_launched_run(
            &paths,
            suite_id,
            "instance",
            "development",
            &manifest_runs,
            0,
            "session-0",
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist launched run");
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": suite_id
        }))
        .expect("deserialize suite tick request");
        let input = request
            .into_suite_plan_input_with_manifest(Some(&paths))
            .expect("suite input should parse");

        let decision = benchmark_suite_driver_decision(&store, input)
            .await
            .expect("tick decision should succeed");

        match decision {
            BenchmarkSuiteDriverDecision::Launch(input) => {
                assert_eq!(input.run_index, 1);
                assert_eq!(input.suite_id, suite_id);
            }
            BenchmarkSuiteDriverDecision::Active { .. }
            | BenchmarkSuiteDriverDecision::Complete { .. } => {
                panic!("pending manifest should launch next run")
            }
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn benchmark_suite_tick_without_manifest_starts_first_run() {
        let store = crate::state::SessionStore::new();
        let root = test_root("suite-tick-no-manifest");
        let paths = test_paths(&root);
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "suite_mode": "development",
            "suite_id": "suite-tick-no-manifest"
        }))
        .expect("deserialize suite tick request");
        let input = request
            .into_suite_plan_input_with_manifest(Some(&paths))
            .expect("suite input should parse");

        let decision = benchmark_suite_driver_decision(&store, input)
            .await
            .expect("tick decision should succeed");

        match decision {
            BenchmarkSuiteDriverDecision::Launch(input) => {
                assert_eq!(input.run_index, 0);
                assert_eq!(input.plan.len(), 2);
            }
            BenchmarkSuiteDriverDecision::Active { .. }
            | BenchmarkSuiteDriverDecision::Complete { .. } => {
                panic!("missing manifest should launch first run")
            }
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_suite_tick_status_payload_excludes_sensitive_fields() {
        let plan = matrix::benchmark_suite_plan("development").expect("development plan");
        let manifest = test_manifest(vec![test_manifest_run(0, Some("session-0"))]);
        let payload = serde_json::json!({
            "status": "active",
            "driver": { "state": "active" },
            "suite": benchmark_suite_driver_status_payload(
                "suite-sensitive",
                "development",
                &plan,
                Some(&manifest),
                Some(1)
            ),
            "active_session_id": bounded_status_token("session-0/C:/Users/Secret --jvm-args")
                .expect("sanitized active session id"),
        });
        let data = serde_json::to_string(&payload).expect("serialize tick payload");
        let lower_data = data.to_ascii_lowercase();

        assert!(data.len() < 2048);
        assert!(!data.contains("SecretUser"));
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("jvm"));
        assert!(!lower_data.contains("username"));
        assert!(!lower_data.contains("filesystem"));
        assert!(!lower_data.contains("args"));
    }

    #[test]
    fn benchmark_suite_metadata_has_no_sensitive_request_fields() {
        let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
            "instance_id": "instance",
            "username": "SecretUser",
            "suite_mode": "release_validation",
            "run_index": 3
        }))
        .expect("deserialize suite request");
        let input = request
            .into_suite_launch_input()
            .expect("suite input should parse");
        let selected = input.plan[input.run_index];
        let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
            Some(benchmark_suite_run_id(&input.mode, input.run_index, selected).as_str()),
            Some(selected.profile),
            Some(selected.run_type),
            Some(input.mode.as_str()),
        );
        let payload = serde_json::json!({
            "benchmark": benchmark_status_payload(&benchmark),
            "suite": benchmark_suite_status_payload(
                &input.suite_id,
                &input.mode,
                input.run_index,
                &input.plan
            ),
        });
        let data = serde_json::to_string(&payload).expect("serialize suite payload");
        let lower_data = data.to_ascii_lowercase();

        assert!(data.len() < 2048);
        assert!(!data.contains("SecretUser"));
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("jvm"));
        assert!(!lower_data.contains("username"));
    }

    #[test]
    fn benchmark_status_payload_uses_sanitized_active_status_shape() {
        let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
            Some(" benchmark-1 "),
            Some(" dev-default "),
            Some(" repeat "),
            Some("release_validation"),
        );

        assert_eq!(
            benchmark_status_payload(&benchmark),
            serde_json::json!({
                "id": "benchmark-1",
                "profile": "dev-default",
                "run_type": "repeat",
                "mode": "release_validation",
            })
        );
    }

    struct RouteTestFixture {
        state: AppState,
        paths: AppPaths,
        root: PathBuf,
    }

    impl RouteTestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            Self::from_root_paths(root, paths)
        }

        fn reload(&self) -> Self {
            Self::from_root_paths(self.root.clone(), self.paths.clone())
        }

        fn from_root_paths(root: PathBuf, paths: AppPaths) -> Self {
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(PerformanceManager::new().expect("performance manager")),
                frontend_dir: root.join("frontend"),
            });

            Self { state, paths, root }
        }

        async fn active_driver(
            &self,
            suite_id: &str,
            mode: &str,
            interval_ms: u64,
        ) -> crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus {
            let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                run_count: 2,
                launched_run_count: 1,
                pending_run_index: Some(1),
            };
            let started = self
                .state
                .benchmark_suite_drivers()
                .start(
                    suite_id.to_string(),
                    mode.to_string(),
                    interval_ms,
                    summary.clone(),
                )
                .await
                .expect("driver starts");
            self.state
                .benchmark_suite_drivers()
                .record_active(
                    &started.status.id,
                    summary,
                    Some("session-before-restart".to_string()),
                )
                .await;
            self.state
                .benchmark_suite_drivers()
                .get(&started.status.id)
                .await
                .expect("active driver status")
        }

        async fn stopped_driver(
            &self,
            suite_id: &str,
            mode: &str,
            interval_ms: u64,
        ) -> crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStatus {
            let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                run_count: 2,
                launched_run_count: 1,
                pending_run_index: Some(1),
            };
            let started = self
                .state
                .benchmark_suite_drivers()
                .start(suite_id.to_string(), mode.to_string(), interval_ms, summary)
                .await
                .expect("driver starts");
            self.state
                .benchmark_suite_drivers()
                .record_stopped(&started.status.id)
                .await;
            self.state
                .benchmark_suite_drivers()
                .get(&started.status.id)
                .await
                .expect("stopped driver status")
        }

        fn persist_suite_runs(&self, suite_id: &str, launched_run_indexes: &[usize]) {
            let plan = matrix::benchmark_suite_plan("development").expect("development plan");
            let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
            for run_index in launched_run_indexes {
                crate::state::benchmark_suites::persist_launched_run(
                    &self.paths,
                    suite_id,
                    "instance",
                    "development",
                    &manifest_runs,
                    *run_index,
                    &format!("session-{run_index}"),
                    "2026-01-01T00:00:00.000Z",
                )
                .expect("persist launched suite run");
            }
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("croopor-launch-{name}-{nanos}"))
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

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    fn persist_family_c_suite_run(
        paths: &AppPaths,
        suite_id: &str,
        run_index: usize,
        session_id: &str,
    ) {
        let plan = matrix::benchmark_suite_plan("release_validation").expect("release plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);
        crate::state::benchmark_suites::persist_launched_run(
            paths,
            suite_id,
            "family-c-instance",
            "release_validation",
            &manifest_runs,
            run_index,
            session_id,
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist launched family c suite run");
    }

    fn write_family_c_suite_manifest(
        paths: &AppPaths,
        manifest: &crate::state::benchmark_suites::BenchmarkSuiteManifest,
    ) {
        let path = crate::state::benchmark_suites::suite_path(paths, &manifest.suite_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create benchmark suite dir");
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(manifest).expect("serialize suite manifest"),
        )
        .expect("write suite manifest");
    }

    fn write_family_c_proof(
        paths: &AppPaths,
        run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
        performance_mode: &str,
        comparison: Option<crate::state::launch_reports::LaunchProofComparison>,
    ) {
        let proof = family_c_proof_record(run, performance_mode, comparison);
        write_family_c_proof_record(paths, &proof);
    }

    fn write_family_c_proof_record(
        paths: &AppPaths,
        proof: &crate::state::launch_reports::LaunchProofRecord,
    ) {
        let path = crate::state::launch_reports::report_path(paths, &proof.session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create launch proof dir");
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(proof).expect("serialize proof"),
        )
        .expect("write launch proof");
    }

    fn family_c_proof_record(
        run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
        performance_mode: &str,
        comparison: Option<crate::state::launch_reports::LaunchProofComparison>,
    ) -> crate::state::launch_reports::LaunchProofRecord {
        let session_id = run.session_id.clone().expect("suite run session id");
        let scenario_id = match performance_mode {
            "vanilla" => "vanilla_launch",
            "managed" => "managed_launch",
            _ => "unknown_launch",
        };

        crate::state::launch_reports::LaunchProofRecord {
            schema: "croopor.launch.proof".to_string(),
            schema_version: 1,
            session_id,
            instance_id: "family-c-instance".to_string(),
            version_id: "1.12.2".to_string(),
            launched_at: "2026-01-01T00:00:00.000Z".to_string(),
            recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            outcome: "completed".to_string(),
            scenario: crate::state::launch_reports::LaunchProofScenario {
                scenario_id: scenario_id.to_string(),
                performance_mode: performance_mode.to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.12.2".to_string()),
                benchmark_profile: Some(run.profile.clone()),
                benchmark_run_type: Some(run.run_type.clone()),
                benchmark_mode: Some("release_validation".to_string()),
                benchmark_id: Some(run.benchmark_id.clone()),
            },
            device: crate::state::launch_reports::LaunchProofDevice {
                tier: "mid".to_string(),
                total_memory_mb: Some(16_384),
                cpu_threads: Some(8),
            },
            resource_budget: None,
            pid: None,
            exit_code: Some(0),
            boot_duration_ms: None,
            priority: None,
            failure_class: None,
            failure_detail: None,
            guardian: None,
            healing: None,
            stages: vec![LaunchStageRecord {
                stage: "launching".to_string(),
                label: "Launching".to_string(),
                started_at_ms: 1_000,
                ended_at_ms: Some(1_100),
                duration_ms: Some(100),
                result: Some("completed".to_string()),
                warnings: Vec::new(),
                fallback_reason: None,
            }],
            comparison,
        }
    }

    fn test_manifest(
        runs: Vec<crate::state::benchmark_suites::BenchmarkSuiteManifestRun>,
    ) -> crate::state::benchmark_suites::BenchmarkSuiteManifest {
        crate::state::benchmark_suites::BenchmarkSuiteManifest {
            schema: "croopor.launch.benchmark.suite".to_string(),
            schema_version: 2,
            suite_id: "suite-test".to_string(),
            instance_id: "instance".to_string(),
            mode: "development".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            runs,
        }
    }

    fn test_manifest_run(
        run_index: usize,
        session_id: Option<&str>,
    ) -> crate::state::benchmark_suites::BenchmarkSuiteManifestRun {
        crate::state::benchmark_suites::BenchmarkSuiteManifestRun {
            run_index,
            profile: "vanilla_baseline".to_string(),
            run_type: "coldish".to_string(),
            target_id: String::new(),
            benchmark_id: format!("suite-development-{run_index:02}-vanilla_baseline-coldish"),
            session_id: session_id.map(str::to_string),
            launched_at: session_id.map(|_| "2026-01-01T00:00:00.000Z".to_string()),
            state: if session_id.is_some() {
                "launching".to_string()
            } else {
                "pending".to_string()
            },
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
}
