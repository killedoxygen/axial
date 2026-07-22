use super::LaunchApplicationError;
use crate::application::performance::{
    self, BenchmarkMatrix, BenchmarkSuiteRunSpec, benchmark_suite_manifest_run_inputs,
    benchmark_suite_run_descriptor, benchmark_suite_run_id,
};
use crate::state::benchmark_suite_drivers::{
    BenchmarkSuiteDriverStartError, BenchmarkSuiteDriverStoreError,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{AppState, LaunchEvent, LaunchStatusEvent};
use axial_launcher::{LaunchSessionOutcomeKind, LaunchStageEvidence, LaunchState};
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
    pub(crate) requested_run_index: Option<usize>,
    pub(crate) plan: Vec<BenchmarkSuiteRunSpec>,
}

#[derive(Debug)]
struct BenchmarkSuiteOwnedLaunch {
    payload: serde_json::Value,
    run_index: usize,
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
    let Ok(producer) = state.try_claim_producer() else {
        return false;
    };
    let state = state.clone();
    let reconciliation_owner = producer.claim_child();
    producer.spawn(async move {
        let cleanup_issues = state.benchmark_suites().retry_terminal_retention().await;
        if !cleanup_issues.is_empty() {
            tracing::warn!(
                pending = cleanup_issues.len(),
                "benchmark suite startup retention cleanup is pending"
            );
        }
        match resume_restart_interrupted_benchmark_suite_drivers(state, reconciliation_owner).await
        {
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
    producer: crate::state::ProducerLease,
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
            Err(BenchmarkSuiteDriverStartError::ShuttingDown) => {
                return Err(BenchmarkSuiteDriverStoreError::ShuttingDown);
            }
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
            &producer,
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

    pub(crate) fn into_suite_launch_input(
        self,
    ) -> Result<BenchmarkSuiteLaunchInput, (StatusCode, Json<serde_json::Value>)> {
        let requested_run_index = self.run_index;
        let input = self.into_suite_plan_input_with_manifest(None)?;

        Ok(BenchmarkSuiteLaunchInput {
            launch: input.launch,
            suite_id: input.suite_id,
            mode: input.mode,
            requested_run_index: requested_run_index
                .map(|run_index| validate_benchmark_suite_run_index(run_index, input.plan.len()))
                .transpose()?,
            plan: input.plan,
        })
    }

    pub(crate) fn into_suite_plan_input_with_manifest(
        self,
        store: Option<&crate::state::benchmark_suites::BenchmarkSuiteStore>,
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
        let manifest = match store {
            Some(store) => store
                .get(&suite_id)
                .map_err(benchmark_suite_store_error_response)?,
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
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let input = payload.into_launch_input()?;
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let launch_owner = producer.claim_child();
    producer.spawn(async move {
        let result = async {
            let mut prepared =
                super::prepare_launch_session_owned(&state, input.launch, &launch_owner).await?;
            let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
                Some(prepared.task.session_id.0.as_str()),
                input.profile.as_deref(),
                input.run_type.as_deref(),
                input.benchmark_mode.as_deref(),
            );
            let benchmark_response = super::launch_benchmark_status_payload(&benchmark);
            prepared.task.benchmark = Some(benchmark.clone());
            let launched = super::launch_session(state.clone(), prepared.task, launch_owner)
                .await
                .map_err(super::launch_request_error_response)?;

            let mut response = benchmark_launch_response(&state, &launched).await?;
            response["benchmark"] = benchmark_response;
            Ok(response)
        }
        .await;
        let _ = result_tx.send(result);
    });
    result_rx
        .await
        .unwrap_or_else(|_| Err(benchmark_suite_storage_error_response()))
}

pub(crate) async fn launch_benchmark_suite(
    state: AppState,
    payload: BenchmarkLaunchRequest,
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let input = payload.into_suite_launch_input()?;
    launch_benchmark_suite_run(state, input, producer).await
}

pub(crate) async fn tick_benchmark_suite(
    state: AppState,
    payload: BenchmarkLaunchRequest,
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let input = payload.into_suite_plan_input_with_manifest(Some(state.benchmark_suites()))?;
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
            let mut payload = launch_benchmark_suite_run(state, input, producer).await?;
            payload["driver"] = json!({ "state": "launched_next" });
            Ok(payload)
        }
    }
}

pub(crate) async fn start_benchmark_suite_driver(
    state: AppState,
    payload: BenchmarkLaunchRequest,
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let interval_ms = clamp_benchmark_suite_driver_interval_ms(payload.interval_ms);
    let input = payload
        .clone()
        .into_suite_plan_input_with_manifest(Some(state.benchmark_suites()))?;
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
        producer,
        state,
        BenchmarkSuiteDriverStart {
            suite_id: input.suite_id,
            mode: input.mode,
            summary,
            request: driver_payload,
            interval_ms,
            resumed_from: None,
        },
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
    let manifest = state
        .benchmark_suites()
        .get(id)
        .map_err(benchmark_suite_store_error_response)?
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

struct BenchmarkSuiteDriverStart {
    suite_id: String,
    mode: String,
    summary: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    resumed_from: Option<String>,
}

pub(crate) async fn resume_benchmark_suite_driver(
    state: AppState,
    id: String,
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let prepared = prepare_benchmark_suite_driver_resume(&state, &id).await?;
    start_owned_benchmark_suite_driver(producer, state, prepared).await
}

async fn prepare_benchmark_suite_driver_resume(
    state: &AppState,
    id: &str,
) -> Result<BenchmarkSuiteDriverStart, LaunchApplicationError> {
    let status = state
        .benchmark_suite_drivers()
        .get(id)
        .await
        .ok_or_else(benchmark_suite_driver_not_found_error)?;
    if !is_terminal_benchmark_suite_driver_state(&status.state) {
        return Err(benchmark_suite_driver_already_active_error());
    }

    let manifest = state
        .benchmark_suites()
        .get(&status.suite_id)
        .map_err(benchmark_suite_store_error_response)?
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
        .into_suite_plan_input_with_manifest(Some(state.benchmark_suites()))?;
    let summary = benchmark_suite_driver_suite_summary(&input);
    if summary.pending_run_index.is_none() {
        return Err(benchmark_suite_complete_error());
    }

    payload.suite_id = Some(input.suite_id.clone());
    payload.suite_mode = Some(input.mode.clone());
    Ok(BenchmarkSuiteDriverStart {
        suite_id: input.suite_id,
        mode: input.mode,
        summary,
        request: payload,
        interval_ms: status.interval_ms,
        resumed_from: Some(status.id),
    })
}

async fn start_owned_benchmark_suite_driver(
    producer: crate::state::ProducerLease,
    state: AppState,
    start: BenchmarkSuiteDriverStart,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let BenchmarkSuiteDriverStart {
        suite_id,
        mode,
        summary,
        request,
        interval_ms,
        resumed_from,
    } = start;
    let (ownership_tx, ownership_rx) = tokio::sync::oneshot::channel();
    let driver_owner = producer.claim_child();
    producer.spawn(async move {
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
        if let Some(previous_id) = resumed_from.as_deref()
            && let Err(error) = state
                .benchmark_suite_drivers()
                .consume_restart_handoff_started(previous_id)
                .await
        {
            tracing::warn!(
                error_class = error.class(),
                "benchmark suite driver restart handoff checkpoint failed"
            );
        }
        let mut response = benchmark_suite_driver_response_payload("scheduled", &started.status);
        if let Some(resumed_from) = resumed_from {
            response["resumed_from"] = json!(resumed_from);
        }
        let _ = ownership_tx.send(Ok(response));
        tokio::task::yield_now().await;
        own_benchmark_suite_driver_loop(
            driver_owner,
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
    producer: crate::state::ProducerLease,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    launch_benchmark_suite_run_owned(state, input, producer)
        .await
        .map(|launched| launched.payload)
}

async fn launch_benchmark_suite_run_owned(
    state: AppState,
    input: BenchmarkSuiteLaunchInput,
    producer: crate::state::ProducerLease,
) -> Result<BenchmarkSuiteOwnedLaunch, (StatusCode, Json<serde_json::Value>)> {
    let BenchmarkSuiteLaunchInput {
        launch,
        suite_id,
        mode,
        requested_run_index,
        plan,
    } = input;
    let manifest_runs = benchmark_suite_manifest_run_inputs(&mode, &plan);
    let selection = state
        .benchmark_suites()
        .select_reservation(
            &suite_id,
            &launch.instance_id,
            &mode,
            &manifest_runs,
            requested_run_index,
        )
        .await
        .map_err(benchmark_suite_store_error_response)?;
    let run_index = selection.run_index();
    let selected = plan[run_index];
    let benchmark_id = benchmark_suite_run_id(&mode, run_index, selected);
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(benchmark_id.as_str()),
        Some(selected.profile),
        Some(selected.run_type),
        Some(mode.as_str()),
    );
    let benchmark_response = super::launch_benchmark_status_payload(&benchmark);
    let suite_response = benchmark_suite_status_payload(&suite_id, &mode, run_index, &plan);
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let launch_owner = producer.claim_child();
    producer.spawn(async move {
        own_benchmark_suite_launch(
            launch_owner,
            state,
            OwnedBenchmarkSuiteLaunchInput {
                launch,
                selection,
                run_index,
                benchmark,
                benchmark_response,
                suite_response,
            },
            result_tx,
        )
        .await;
    });

    result_rx
        .await
        .unwrap_or_else(|_| Err(benchmark_suite_storage_error_response()))
}

struct OwnedBenchmarkSuiteLaunchInput {
    launch: super::LaunchRequest,
    selection: crate::state::benchmark_suites::BenchmarkSuiteSelection,
    run_index: usize,
    benchmark: crate::state::launch_reports::LaunchBenchmarkMetadata,
    benchmark_response: serde_json::Value,
    suite_response: serde_json::Value,
}

async fn own_benchmark_suite_launch(
    producer: crate::state::ProducerLease,
    state: AppState,
    input: OwnedBenchmarkSuiteLaunchInput,
    result_tx: tokio::sync::oneshot::Sender<
        Result<BenchmarkSuiteOwnedLaunch, LaunchApplicationError>,
    >,
) {
    let OwnedBenchmarkSuiteLaunchInput {
        launch,
        selection,
        run_index,
        benchmark,
        benchmark_response,
        suite_response,
    } = input;
    let mut prepared = match super::prepare_launch_session_owned(&state, launch, &producer).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = result_tx.send(Err(error));
            return;
        }
    };
    let session_id = prepared.task.session_id.0.clone();
    prepared.task.benchmark = Some(benchmark.clone());
    if state
        .sessions()
        .attach_benchmark(
            &session_id,
            super::launch_benchmark_status_payload(&benchmark),
        )
        .await
        .is_none()
    {
        tracing::warn!("prepared benchmark suite session disappeared before reservation");
        finish_benchmark_suite_reservation_failure(
            &state,
            &producer,
            &prepared.task,
            benchmark,
            BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE,
            "prepared_session_missing",
            true,
        )
        .await;
        let _ = result_tx.send(Err(benchmark_suite_storage_error_response()));
        return;
    }
    let Some(terminal_events) = state.sessions().subscribe(&session_id).await else {
        tracing::warn!("prepared benchmark suite session could not be observed");
        finish_benchmark_suite_reservation_failure(
            &state,
            &producer,
            &prepared.task,
            benchmark,
            BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE,
            "prepared_session_unobservable",
            true,
        )
        .await;
        let _ = result_tx.send(Err(benchmark_suite_storage_error_response()));
        return;
    };
    let displaced_session_active = match selection.displaced_session_id() {
        Some(displaced_session_id) => state
            .sessions()
            .get(displaced_session_id)
            .await
            .is_some_and(|record| {
                !matches!(record.state, LaunchState::Failed | LaunchState::Exited)
            }),
        None => false,
    };
    let reservation = state
        .benchmark_suites()
        .reserve(
            selection,
            &session_id,
            &prepared.task.launched_at,
            displaced_session_active,
        )
        .await;
    match reservation {
        Ok(_) => {}
        Err(crate::state::benchmark_suites::BenchmarkSuiteReserveError::PreAccept(error)) => {
            let failure = BenchmarkSuiteReservationFailure::from_store_error(&error);
            finish_benchmark_suite_reservation_failure(
                &state,
                &producer,
                &prepared.task,
                benchmark,
                failure.message,
                error.class(),
                true,
            )
            .await;
            let _ = result_tx.send(Err(failure.response()));
            return;
        }
        Err(crate::state::benchmark_suites::BenchmarkSuiteReserveError::AcceptedWriteFailed {
            handle,
            ..
        }) => {
            finish_benchmark_suite_reservation_failure(
                &state,
                &producer,
                &prepared.task,
                benchmark,
                BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE,
                "accepted_write_failed",
                false,
            )
            .await;
            let _ = result_tx.send(Err(benchmark_suite_storage_error_response()));
            match state.benchmark_suites().settle_compensation(&handle).await {
                Ok(()) => {
                    state
                        .sessions()
                        .release_terminal_retention_hold(&session_id)
                        .await;
                }
                Err(error) => tracing::warn!(
                    error_class = error.class(),
                    "benchmark suite compensation remained unsettled; terminal hold retained"
                ),
            }
            return;
        }
    }
    let launch_owner = producer.claim_child();
    let launched = match super::launch_session(state.clone(), prepared.task, launch_owner).await {
        Ok(launched) => launched,
        Err(error) => {
            let _ = result_tx.send(Err(super::launch_request_error_response(error)));
            return;
        }
    };

    let mut response = match benchmark_launch_response(&state, &launched).await {
        Ok(response) => response,
        Err(error) => {
            let _ = result_tx.send(Err(error));
            return;
        }
    };
    response["benchmark"] = benchmark_response;
    response["suite"] = suite_response;
    let _ = result_tx.send(Ok(BenchmarkSuiteOwnedLaunch {
        payload: response,
        run_index,
    }));
    own_benchmark_suite_terminal_outcome(state, session_id, terminal_events).await;
}

async fn benchmark_launch_response(
    state: &AppState,
    launched: &super::LaunchSuccess,
) -> Result<serde_json::Value, LaunchApplicationError> {
    let status = state
        .sessions()
        .status_snapshot(&launched.session_id)
        .await
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "Could not read the launch status. Try again from the launcher."
                })),
            )
        })?;
    let mut response = json!(super::public_launch_status(&status));
    response["instance_id"] = json!(&launched.instance_id);
    response["launched_at"] = json!(&launched.launched_at);
    response["max_memory_mb"] = json!(launched.max_memory_mb);
    response["min_memory_mb"] = json!(launched.min_memory_mb);
    Ok(response)
}

async fn finish_benchmark_suite_reservation_failure(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    task: &super::LaunchSessionTask,
    benchmark: crate::state::launch_reports::LaunchBenchmarkMetadata,
    message: &'static str,
    error_class: &'static str,
    release_retention_hold: bool,
) {
    let session_id = task.session_id.0.as_str();
    let mut initial_evidence = super::launch_application_stage_evidence();
    initial_evidence.extend(task.preflight_stage_evidence.clone());
    state
        .sessions()
        .record_stage_evidence(session_id, initial_evidence)
        .await;
    state
        .sessions()
        .emit_log(session_id, "system", message.to_string())
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
                failure_detail: Some(message.to_string()),
                crash_evidence: None,
                healing: None,
                guardian: serde_json::to_value(&task.guardian).ok(),
                outcome: None,
                notice: None,
                evidence: vec![LaunchStageEvidence {
                    id: "application_benchmark_suite_reservation_failed".to_string(),
                    system: "application".to_string(),
                    summary: "Benchmark suite reservation failed before process start.".to_string(),
                    details: vec![format!("reason:{error_class}")],
                }],
                stages: Vec::new(),
            },
        )
        .await;

    let proof_context = LaunchProofContext::from_intent(&task.intent, &task.performance_mode)
        .with_benchmark(Some(benchmark))
        .with_resource_budget(task.resource_budget.clone());
    super::runner::persist_launch_proof_for_reservation_failure(
        state,
        producer,
        session_id,
        Some(task.launched_at.as_str()),
        &proof_context,
    )
    .await;
    if release_retention_hold {
        state
            .sessions()
            .release_terminal_retention_hold(session_id)
            .await;
    }
}

async fn own_benchmark_suite_terminal_outcome(
    state: AppState,
    session_id: String,
    mut terminal_events: tokio::sync::broadcast::Receiver<LaunchEvent>,
) {
    loop {
        match terminal_events.recv().await {
            Ok(LaunchEvent::Status(status)) => {
                let Some(outcome) = benchmark_suite_terminal_outcome(&status) else {
                    continue;
                };
                persist_benchmark_suite_terminal_outcome(&state, &session_id, outcome).await;
                return;
            }
            Ok(LaunchEvent::Log(_)) => {}
            Ok(LaunchEvent::ProcessSettled { .. }) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let Some(record) = state.sessions().get(&session_id).await else {
                    tracing::warn!(
                        "benchmark suite terminal observer lost its exact session record"
                    );
                    return;
                };
                let outcome = benchmark_suite_record_terminal_outcome(&record);
                if let Some(outcome) = outcome {
                    persist_benchmark_suite_terminal_outcome(&state, &session_id, outcome).await;
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let outcome = state
                    .sessions()
                    .get(&session_id)
                    .await
                    .and_then(|record| benchmark_suite_record_terminal_outcome(&record));
                if let Some(outcome) = outcome {
                    persist_benchmark_suite_terminal_outcome(&state, &session_id, outcome).await;
                } else {
                    tracing::warn!(
                        "benchmark suite terminal observer closed before exact settlement"
                    );
                }
                return;
            }
        }
    }
}

fn benchmark_suite_terminal_outcome(status: &LaunchStatusEvent) -> Option<&'static str> {
    status
        .outcome
        .as_ref()
        .map(|outcome| benchmark_suite_outcome_kind(outcome.kind))
        .or_else(|| benchmark_suite_state_only_terminal_outcome(&status.state))
}

fn benchmark_suite_record_terminal_outcome(
    record: &crate::state::LaunchSessionRecord,
) -> Option<&'static str> {
    record
        .outcome
        .as_ref()
        .map(|outcome| benchmark_suite_outcome_kind(outcome.kind))
        .or(match record.state {
            LaunchState::Failed => Some("failed"),
            LaunchState::Exited => Some("exited"),
            _ => None,
        })
}

fn benchmark_suite_outcome_kind(kind: LaunchSessionOutcomeKind) -> &'static str {
    match kind {
        LaunchSessionOutcomeKind::Clean | LaunchSessionOutcomeKind::Unknown => "exited",
        LaunchSessionOutcomeKind::Stopped => "stopped",
        LaunchSessionOutcomeKind::Failed => "failed",
    }
}

fn benchmark_suite_state_only_terminal_outcome(state: &str) -> Option<&'static str> {
    match state.trim() {
        "failed" => Some("failed"),
        "stopped" => Some("stopped"),
        "exited" => Some("exited"),
        _ => None,
    }
}

async fn persist_benchmark_suite_terminal_outcome(
    state: &AppState,
    session_id: &str,
    outcome: &str,
) {
    if let Err(error) = state
        .benchmark_suites()
        .update_run_state_for_session(session_id, outcome)
        .await
    {
        tracing::warn!(
            error_class = error.class(),
            "benchmark suite terminal outcome persistence failed"
        );
    }
}

fn benchmark_suite_storage_error_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })),
    )
}

pub(crate) fn benchmark_suite_store_error_response(
    error: crate::state::benchmark_suites::BenchmarkSuiteStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    BenchmarkSuiteReservationFailure::from_store_error(&error).response()
}

#[derive(Clone, Copy)]
struct BenchmarkSuiteReservationFailure {
    status: StatusCode,
    message: &'static str,
}

impl BenchmarkSuiteReservationFailure {
    fn from_store_error(error: &crate::state::benchmark_suites::BenchmarkSuiteStoreError) -> Self {
        use crate::state::benchmark_suites::BenchmarkSuiteStoreError;
        match error {
            BenchmarkSuiteStoreError::InvalidSuiteId => Self {
                status: StatusCode::BAD_REQUEST,
                message: "benchmark suite id is invalid",
            },
            BenchmarkSuiteStoreError::InvalidRunIndex => Self {
                status: StatusCode::BAD_REQUEST,
                message: "run_index is out of range",
            },
            BenchmarkSuiteStoreError::SuiteIdentityMismatch => Self {
                status: StatusCode::CONFLICT,
                message: "benchmark suite does not match this instance and mode",
            },
            BenchmarkSuiteStoreError::AutoConflict
            | BenchmarkSuiteStoreError::ExplicitActiveConflict => Self {
                status: StatusCode::CONFLICT,
                message: "benchmark suite has active run",
            },
            BenchmarkSuiteStoreError::StaleSelection
            | BenchmarkSuiteStoreError::SessionConflict => Self {
                status: StatusCode::CONFLICT,
                message: "benchmark suite changed before launch",
            },
            BenchmarkSuiteStoreError::Complete => Self {
                status: StatusCode::CONFLICT,
                message: "benchmark suite is complete",
            },
            BenchmarkSuiteStoreError::RejectedManifest
            | BenchmarkSuiteStoreError::MutationLatched
            | BenchmarkSuiteStoreError::ProofCapacity
            | BenchmarkSuiteStoreError::RetryRequired
            | BenchmarkSuiteStoreError::Closed
            | BenchmarkSuiteStoreError::GenerationOverflow
            | BenchmarkSuiteStoreError::ObligationOverflow
            | BenchmarkSuiteStoreError::Cleanup(_)
            | BenchmarkSuiteStoreError::Persistence(_) => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE,
            },
        }
    }

    fn response(self) -> (StatusCode, Json<serde_json::Value>) {
        (self.status, Json(json!({ "error": self.message })))
    }
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
        BenchmarkSuiteDriverStartError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "benchmark suite drivers are shutting down" })),
        ),
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
        | BenchmarkSuiteDriverStoreError::RetentionConflict
        | BenchmarkSuiteDriverStoreError::IdExhausted
        | BenchmarkSuiteDriverStoreError::Persistence(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "benchmark suite driver state could not be persisted"
            })),
        ),
        BenchmarkSuiteDriverStoreError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "benchmark suite drivers are shutting down" })),
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

pub(crate) fn unsupported_suite_mode_error() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "suite_mode is not supported" })),
    )
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

    if pending_run_index.is_none() {
        return Ok(BenchmarkSuiteDriverDecision::Complete { suite });
    }

    Ok(BenchmarkSuiteDriverDecision::Launch(
        BenchmarkSuiteLaunchInput {
            launch: input.launch,
            suite_id: input.suite_id,
            mode: input.mode,
            requested_run_index: None,
            plan: input.plan,
        },
    ))
}

pub(crate) fn spawn_benchmark_suite_driver_loop(
    producer: &crate::state::ProducerLease,
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    effect_owner: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverEffectOwner,
) {
    let driver_owner = producer.claim_child();
    producer.spawn_child(async move {
        own_benchmark_suite_driver_loop(
            driver_owner,
            state,
            driver_id,
            request,
            interval_ms,
            effect_owner,
        )
        .await;
    });
}

async fn own_benchmark_suite_driver_loop(
    producer: crate::state::ProducerLease,
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    effect_owner: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverEffectOwner,
) {
    let stop_rx = effect_owner.stop_receiver();
    let shutdown = state.subscribe_shutdown();
    match run_benchmark_suite_driver_loop(
        &producer,
        state,
        driver_id,
        request,
        interval_ms,
        stop_rx,
        shutdown,
    )
    .await
    {
        Ok(())
        | Err(BenchmarkSuiteDriverStoreError::TerminalDriver)
        | Err(BenchmarkSuiteDriverStoreError::ShuttingDown) => {}
        Err(error) => tracing::warn!(
            error_class = error.class(),
            "benchmark suite driver persistence failed"
        ),
    }
    drop(effect_owner);
}

pub(crate) async fn run_benchmark_suite_driver_loop(
    producer: &crate::state::ProducerLease,
    state: AppState,
    driver_id: String,
    request: BenchmarkLaunchRequest,
    interval_ms: u64,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), BenchmarkSuiteDriverStoreError> {
    // Stop requests are observed between launches so an in-flight benchmark can finish cleanly.
    loop {
        if *stop_rx.borrow() || *shutdown.borrow() {
            break;
        }

        let input = match request
            .clone()
            .into_suite_plan_input_with_manifest(Some(state.benchmark_suites()))
        {
            Ok(input) => input,
            Err(error) => {
                if *shutdown.borrow() {
                    break;
                }
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
                if *stop_rx.borrow() || *shutdown.borrow() {
                    break;
                }
                match launch_benchmark_suite_run_owned(state.clone(), input, producer.claim_child())
                    .await
                {
                    Ok(launched) => {
                        let session_id = launched
                            .payload
                            .get("session_id")
                            .and_then(|value| value.as_str())
                            .and_then(bounded_status_token);
                        let summary = request
                            .clone()
                            .into_suite_plan_input_with_manifest(Some(state.benchmark_suites()))
                            .map(|input| benchmark_suite_driver_suite_summary(&input))
                            .unwrap_or(summary);
                        state
                            .benchmark_suite_drivers()
                            .record_launched(&driver_id, summary, launched.run_index, session_id)
                            .await?;
                    }
                    Err(error) => {
                        if *shutdown.borrow() {
                            break;
                        }
                        state
                            .benchmark_suite_drivers()
                            .record_failed(&driver_id, &benchmark_suite_api_error_message(&error))
                            .await?;
                        break;
                    }
                }
            }
            Err(error) => {
                if *shutdown.borrow() {
                    break;
                }
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
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
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
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_launcher::{LaunchSessionRecord, SessionId};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use axial_performance::PerformanceManager;
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::task::{Context, Poll};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn startup_driver_resume_is_not_admitted_after_quiescence() {
        let fixture = BenchmarkFixture::new("startup-resume-after-quiescence");
        fixture.state.quiesce().await.expect("quiesce state");

        assert!(!spawn_restart_interrupted_benchmark_suite_drivers(
            &fixture.state
        ));
    }

    struct FailingReservationBackend {
        attempts: AtomicUsize,
        started: Notify,
        first_gate: BlockingGate,
        compensation_gate: BlockingGate,
    }

    struct BlockingGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct GateRelease<'a> {
        gates: Vec<&'a BlockingGate>,
    }

    impl Drop for GateRelease<'_> {
        fn drop(&mut self) {
            for gate in &self.gates {
                gate.release();
            }
        }
    }

    impl FailingReservationBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                started: Notify::new(),
                first_gate: BlockingGate::new(),
                compensation_gate: BlockingGate::new(),
            }
        }

        async fn wait_for_attempt(&self, expected: usize) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let started = self.started.notified();
                    if self.attempts.load(Ordering::SeqCst) >= expected {
                        return;
                    }
                    started.await;
                }
            })
            .await
            .expect("reservation persistence attempt starts");
        }
    }

    impl BlockingGate {
        fn new() -> Self {
            Self {
                released: Mutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("write gate wait");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }
    }

    impl AtomicWriteBackend for FailingReservationBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            self.started.notify_waiters();
            match attempt {
                1 => {
                    self.first_gate.wait();
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "secret reservation path C:\\Users\\Alice\\suite.json",
                    ))
                }
                2 => {
                    self.compensation_gate.wait();
                    destination.write(effects, contents)
                }
                _ => {
                    destination.write(effects, contents)
                }
            }
        }
    }

    #[tokio::test]
    async fn benchmark_suite_preaccept_failure_finalizes_and_releases_prepared_session() {
        let fixture = BenchmarkFixture::new("reservation-finalizes-session");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Benchmark", "1.21.1");
        let suite_id = crate::state::benchmark_suites::derive_suite_id(&instance_id, "development");
        let plan = performance::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        let stale_selection = fixture
            .state
            .benchmark_suites()
            .select_reservation(&suite_id, &instance_id, "development", &manifest_runs, None)
            .await
            .expect("select reservation under test");
        let competing_selection = fixture
            .state
            .benchmark_suites()
            .select_reservation(&suite_id, &instance_id, "development", &manifest_runs, None)
            .await
            .expect("select competing reservation");
        fixture
            .state
            .benchmark_suites()
            .reserve(
                competing_selection,
                "competing-session",
                "2026-01-01T00:00:00.000Z",
                false,
            )
            .await
            .expect("commit competing reservation");
        let selected_run_index = stale_selection.run_index();
        let selected = plan[selected_run_index];
        let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
            Some(&benchmark_suite_run_id(
                "development",
                selected_run_index,
                selected,
            )),
            Some(selected.profile),
            Some(selected.run_type),
            Some("development"),
        );
        let benchmark_response = super::super::launch_benchmark_status_payload(&benchmark);
        let suite_response =
            benchmark_suite_status_payload(&suite_id, "development", selected_run_index, &plan);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let producer = fixture
            .state
            .try_claim_producer()
            .expect("claim benchmark owner");

        own_benchmark_suite_launch(
            producer,
            fixture.state.clone(),
            OwnedBenchmarkSuiteLaunchInput {
                launch: super::super::LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                selection: stale_selection,
                run_index: selected_run_index,
                benchmark,
                benchmark_response,
                suite_response,
            },
            result_tx,
        )
        .await;
        let error = result_rx
            .await
            .expect("owner reports result")
            .expect_err("stale suite selection should reject reservation");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            json!({ "error": "benchmark suite has active run" })
        );
        let proofs = fixture.state.launch_reports().list_recent(5);
        assert_eq!(proofs.len(), 1);
        let proof = &proofs[0];
        assert_eq!(proof.outcome, "failed");
        assert_eq!(
            proof.scenario.benchmark_mode.as_deref(),
            Some("development")
        );
        assert_eq!(
            proof.failure_detail.as_deref(),
            Some("benchmark suite has active run")
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
        assert_eq!(
            record
                .stages
                .iter()
                .flat_map(|stage| &stage.evidence)
                .filter(|evidence| {
                    evidence.id == "guardian_launch_safety_decision"
                        && evidence.system == "guardian"
                })
                .count(),
            1
        );
        assert!(fixture.state.sessions().active_records().await.is_empty());

        for index in 0..=32 {
            let completed_id = format!("completed-{index}");
            fixture
                .state
                .sessions()
                .insert(test_record(&completed_id))
                .await
                .expect("insert session");
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_reservation_failure_holds_session_until_exact_compensation() {
        let mut fixture = BenchmarkFixture::new("accepted-reservation-compensation");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Benchmark", "1.21.1");
        fixture
            .state
            .benchmark_suites()
            .close()
            .await
            .expect("close default suite store");
        let backend = Arc::new(FailingReservationBackend::new());
        let _gate_release = GateRelease {
            gates: vec![&backend.first_gate, &backend.compensation_gate],
        };
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let suite_store = Arc::new(
            crate::state::benchmark_suites::BenchmarkSuiteStore::try_load_from_paths_with_coordinator(
                &fixture.paths,
                coordinator,
            )
            .expect("load injected suite store"),
        );
        fixture.state = fixture.state.clone().with_benchmark_suites(suite_store);
        let report_dir = fixture.paths.launch_reports_dir();
        fs::create_dir_all(report_dir.parent().expect("report parent"))
            .expect("create report parent");
        fs::write(&report_dir, b"not a directory").expect("block proof directory");
        let plan = performance::benchmark_suite_plan("development").expect("development plan");
        let suite_id = crate::state::benchmark_suites::derive_suite_id(&instance_id, "development");
        let state = fixture.state.clone();
        let producer = state.try_claim_producer().expect("claim suite producer");
        let waiter = tokio::spawn(launch_benchmark_suite_run(
            state.clone(),
            BenchmarkSuiteLaunchInput {
                launch: super::super::LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                suite_id: suite_id.clone(),
                mode: "development".to_string(),
                requested_run_index: None,
                plan,
            },
            producer,
        ));

        backend.wait_for_attempt(1).await;
        let prepared = state.sessions().active_records().await;
        assert_eq!(prepared.len(), 1);
        let session_id = prepared[0].session_id.0.clone();
        backend.first_gate.release();
        backend.wait_for_attempt(2).await;
        let error = waiter
            .await
            .expect("suite launch waiter joins")
            .expect_err("accepted reservation write fails");

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error.1.0,
            json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })
        );
        let terminal = state
            .sessions()
            .get(&session_id)
            .await
            .expect("terminal session retained");
        assert_eq!(terminal.state, LaunchState::Failed);
        assert_eq!(
            state.sessions().retention_hold_count(&session_id).await,
            Some(1)
        );
        assert!(
            state
                .benchmark_suites()
                .get(&suite_id)
                .expect("read committed suite")
                .is_none()
        );
        assert_eq!(
            terminal
                .failure
                .as_ref()
                .and_then(|failure| failure.detail.as_deref()),
            Some(BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE)
        );
        assert!(terminal.command.is_empty());
        assert!(terminal.java_path.is_none());
        assert!(terminal.natives_dir.is_none());

        backend.compensation_gate.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.sessions().retention_hold_count(&session_id).await == Some(0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("compensation releases terminal hold");
        state
            .benchmark_suites()
            .close()
            .await
            .expect("close compensated suite store");
        let reloaded = crate::state::benchmark_suites::BenchmarkSuiteStore::load_from_paths(
            &fixture.paths,
            crate::state::benchmark_suites::BenchmarkSuiteRetentionClaims::default(),
        );
        let manifest = reloaded
            .get(&suite_id)
            .expect("read reloaded suite")
            .expect("compensation checkpoint exists");
        assert!(
            manifest
                .runs
                .iter()
                .all(|run| run.state == "pending" && run.session_id.is_none())
        );
        reloaded.close().await.expect("close reloaded suite store");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_suite_launch_waiter_does_not_cancel_owned_continuation() {
        let mut fixture = BenchmarkFixture::new("aborted-suite-launch-waiter");
        fixture.write_ready_install("1.21.1");
        let instance_id = fixture.add_instance("Benchmark", "1.21.1");
        fixture
            .state
            .benchmark_suites()
            .close()
            .await
            .expect("close default suite store");
        let backend = Arc::new(FailingReservationBackend::new());
        let _gate_release = GateRelease {
            gates: vec![&backend.first_gate, &backend.compensation_gate],
        };
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let suite_store = Arc::new(
            crate::state::benchmark_suites::BenchmarkSuiteStore::try_load_from_paths_with_coordinator(
                &fixture.paths,
                coordinator,
            )
            .expect("load injected suite store"),
        );
        fixture.state = fixture.state.clone().with_benchmark_suites(suite_store);
        let suite_id = crate::state::benchmark_suites::derive_suite_id(&instance_id, "development");
        let state = fixture.state.clone();
        let producer = state.try_claim_producer().expect("claim suite producer");
        let waiter = tokio::spawn(launch_benchmark_suite_run(
            state.clone(),
            BenchmarkSuiteLaunchInput {
                launch: super::super::LaunchRequest {
                    instance_id,
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                },
                suite_id: suite_id.clone(),
                mode: "development".to_string(),
                requested_run_index: None,
                plan: performance::benchmark_suite_plan("development").expect("development plan"),
            },
            producer,
        ));

        backend.wait_for_attempt(1).await;
        let prepared = state.sessions().active_records().await;
        assert_eq!(prepared.len(), 1);
        let session_id = prepared[0].session_id.0.clone();
        waiter.abort();
        let _ = waiter.await;
        backend.first_gate.release();
        backend.wait_for_attempt(2).await;

        let terminal = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = state.sessions().get(&session_id).await
                    && record.state == LaunchState::Failed
                {
                    break record;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("owned continuation terminalizes after waiter abort");
        assert_eq!(
            terminal.outcome.as_ref().map(|outcome| outcome.kind),
            Some(LaunchSessionOutcomeKind::Unknown)
        );
        assert_eq!(
            state.sessions().retention_hold_count(&session_id).await,
            Some(1)
        );
        backend.compensation_gate.release();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let manifest = state
                    .benchmark_suites()
                    .get(&suite_id)
                    .expect("read committed suite");
                if manifest.as_ref().is_some_and(|manifest| {
                    manifest
                        .runs
                        .iter()
                        .all(|run| run.state == "pending" && run.session_id.is_none())
                }) && state.sessions().retention_hold_count(&session_id).await == Some(0)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("owned continuation commits terminal outcome");
        let manifest = state
            .benchmark_suites()
            .get(&suite_id)
            .expect("read committed suite")
            .expect("suite compensation committed");
        assert!(
            manifest
                .runs
                .iter()
                .all(|run| run.state == "pending" && run.session_id.is_none())
        );
        assert_eq!(
            state.sessions().retention_hold_count(&session_id).await,
            Some(0)
        );
    }

    #[tokio::test]
    async fn natural_terminal_session_releases_suite_for_next_auto_selection() {
        let fixture = BenchmarkFixture::new("natural-terminal-suite-outcome");
        let state = fixture.state.clone();
        let instance_id = "instance";
        let suite_id = crate::state::benchmark_suites::derive_suite_id(instance_id, "development");
        let session_id = "natural-terminal-session";
        let plan = performance::benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        let selection = state
            .benchmark_suites()
            .select_reservation(&suite_id, instance_id, "development", &manifest_runs, None)
            .await
            .expect("select first run");
        state
            .benchmark_suites()
            .reserve(selection, session_id, "2026-01-01T00:00:00.000Z", false)
            .await
            .expect("reserve first run");
        state
            .benchmark_suites()
            .update_run_state_for_session(session_id, "running")
            .await
            .expect("commit running outcome");
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let terminal_events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe exact session");
        let terminal_owner = tokio::spawn(own_benchmark_suite_terminal_outcome(
            state.clone(),
            session_id.to_string(),
            terminal_events,
        ));

        state
            .sessions()
            .emit_status(session_id, terminal_status())
            .await;

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let manifest = state
                    .benchmark_suites()
                    .get(&suite_id)
                    .expect("read committed suite")
                    .expect("suite exists");
                if manifest.runs[0].state == "exited" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal continuation commits suite outcome");
        terminal_owner.await.expect("terminal owner joins");
        let next = state
            .benchmark_suites()
            .select_reservation(&suite_id, instance_id, "development", &manifest_runs, None)
            .await
            .expect("auto selection advances after natural exit");
        assert_eq!(next.run_index(), 1);
    }

    #[test]
    fn terminal_suite_outcome_preserves_stop_crash_and_clean_classification() {
        let mut status = terminal_status();
        status.outcome = Some(axial_launcher::LaunchSessionOutcome::from_reason(
            axial_launcher::LaunchSessionExitReason::LauncherStopped,
        ));
        assert_eq!(benchmark_suite_terminal_outcome(&status), Some("stopped"));

        status.outcome = Some(axial_launcher::LaunchSessionOutcome::from_reason(
            axial_launcher::LaunchSessionExitReason::CrashedAfterBoot,
        ));
        assert_eq!(benchmark_suite_terminal_outcome(&status), Some("failed"));

        status.outcome = Some(axial_launcher::LaunchSessionOutcome::from_reason(
            axial_launcher::LaunchSessionExitReason::CleanExit,
        ));
        assert_eq!(benchmark_suite_terminal_outcome(&status), Some("exited"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceled_start_waiter_does_not_drop_detached_effect_owner() {
        let fixture = BenchmarkFixture::new("canceled-driver-start-waiter");
        let state = fixture.state.clone();
        let suite_id =
            crate::state::benchmark_suites::derive_suite_id("missing-instance", "development");
        let producer = state.try_claim_producer().expect("claim driver producer");
        let mut waiter = Box::pin(start_owned_benchmark_suite_driver(
            producer,
            state.clone(),
            BenchmarkSuiteDriverStart {
                suite_id: suite_id.clone(),
                mode: "development".to_string(),
                summary: crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                    run_count: 2,
                    launched_run_count: 0,
                    pending_run_index: Some(0),
                },
                request: BenchmarkLaunchRequest {
                    instance_id: Some("missing-instance".to_string()),
                    username: None,
                    max_memory_mb: None,
                    min_memory_mb: None,
                    client_started_at_ms: None,
                    profile: None,
                    run_type: None,
                    benchmark_mode: None,
                    suite_mode: Some("development".to_string()),
                    suite_id: Some(suite_id.clone()),
                    run_index: None,
                    interval_ms: Some(30_000),
                },
                interval_ms: 30_000,
                resumed_from: None,
            },
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
        assert_eq!(failed.suite_id, suite_id);

        let successor = state
            .benchmark_suite_drivers()
            .start(
                failed.suite_id,
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
            fs::create_dir_all(paths.library_dir()).expect("create library dir");
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::from_config(
                    paths.clone(),
                    Arc::clone(&root_session),
                    AppConfig {
                        library_dir: paths.library_dir().to_string_lossy().to_string(),
                        ..AppConfig::default()
                    },
                )
                .expect("set library dir"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
                .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(paths.performance_dir())
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
            });
            Self { state, paths, root }
        }

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            let instance = self
                .state
                .instances()
                .insert_for_test(name.to_string(), version_id.to_string())
                .expect("add instance");
            self.activate_ready_inventory(&instance.id, version_id);
            instance.id
        }

        fn activate_ready_inventory(&self, instance_id: &str, version_id: &str) {
            let version_dir = self.paths.library_dir().join("versions").join(version_id);
            let version_json = version_dir.join(format!("{version_id}.json"));
            let client_jar = version_dir.join(format!("{version_id}.jar"));
            let managed_root = self
                .state
                .managed_runtime_cache()
                .component_root("java-runtime-delta")
                .expect("runtime root");
            let java_path = if cfg!(target_os = "windows") {
                managed_root.join("bin").join("javaw.exe")
            } else if cfg!(target_os = "macos") {
                managed_root.join("jre.bundle/Contents/Home/bin/java")
            } else {
                managed_root.join("bin").join("java")
            };
            let java_relative = java_path
                .strip_prefix(&managed_root)
                .expect("managed Java relative path")
                .to_string_lossy()
                .replace('\\', "/");
            let runtime_root = || TestKnownGoodRoot::ManagedRuntime {
                component: "java-runtime-delta".to_string(),
            };
            let file = |root, path, kind, physical_path: &Path| TestKnownGoodEntry {
                root,
                path,
                kind,
                integrity: TestKnownGoodIntegrity::File {
                    size: fs::metadata(physical_path).expect("known-good file").len(),
                },
            };
            let entries = vec![
                file(
                    TestKnownGoodRoot::Versions,
                    format!("{version_id}/{version_id}.json"),
                    KnownGoodArtifactKind::VersionMetadata,
                    &version_json,
                ),
                file(
                    TestKnownGoodRoot::Versions,
                    format!("{version_id}/{version_id}.jar"),
                    KnownGoodArtifactKind::ClientJar,
                    &client_jar,
                ),
                file(
                    runtime_root(),
                    ".axial-runtime-manifest.json".to_string(),
                    KnownGoodArtifactKind::RuntimeManifestProof,
                    &managed_root.join(".axial-runtime-manifest.json"),
                ),
                file(
                    runtime_root(),
                    ".axial-ready".to_string(),
                    KnownGoodArtifactKind::RuntimeReadyMarker,
                    &managed_root.join(".axial-ready"),
                ),
                file(
                    runtime_root(),
                    java_relative,
                    KnownGoodArtifactKind::RuntimeExecutable,
                    &java_path,
                ),
            ];
            self.state.activate_known_good_inventory_for_test(
                instance_id,
                KnownGoodInventory::from_test_entries(entries).expect("ready inventory"),
            );
        }

        fn write_ready_install(&self, version_id: &str) {
            let version_dir = self.paths.library_dir().join("versions").join(version_id);
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

            let runtime_root = self
                .state
                .managed_runtime_cache()
                .component_root("java-runtime-delta")
                .expect("runtime root");
            let java_path = if cfg!(target_os = "windows") {
                runtime_root.join("bin").join("javaw.exe")
            } else if cfg!(target_os = "macos") {
                runtime_root.join("jre.bundle/Contents/Home/bin/java")
            } else {
                runtime_root.join("bin").join("java")
            };
            fs::create_dir_all(java_path.parent().expect("runtime bin")).expect("runtime bin");
            let java_bytes = b"java";
            fs::write(&java_path, java_bytes).expect("runtime java");
            make_executable(&java_path);
            let java_relative = java_path
                .strip_prefix(&runtime_root)
                .expect("runtime Java relative path")
                .to_string_lossy()
                .replace('\\', "/");
            let manifest = json!({
                "files": {
                    java_relative: {
                        "type": "file",
                        "downloads": {
                            "raw": {
                                "url": "https://example.invalid/java",
                                "sha1": hex::encode(Sha1::digest(java_bytes)),
                                "size": java_bytes.len()
                            }
                        }
                    }
                }
            });
            fs::write(
                runtime_root.join(".axial-runtime-manifest.json"),
                serde_json::to_vec(&manifest).expect("runtime manifest JSON"),
            )
            .expect("runtime proof");
            fs::write(runtime_root.join(".axial-ready"), b"ready").expect("runtime ready marker");
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
            crash_evidence: None,
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
            crash_evidence: None,
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
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
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
