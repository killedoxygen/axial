mod stream;

use crate::application::launch as launch_app;
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    routing::{get, post},
};

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
        .route(
            "/api/v1/launch/preflight/{instance_id}",
            get(handle_launch_preflight),
        )
        .route("/api/v1/launch/reports", get(handle_launch_reports))
        .route("/api/v1/launch/reports/{id}", get(handle_launch_report))
        .route("/api/v1/launch/{id}/events", get(handle_launch_events))
        .route("/api/v1/launch/{id}/status", get(handle_launch_status))
        .route("/api/v1/launch/{id}/command", get(handle_launch_command))
        .route("/api/v1/launch/{id}/kill", post(handle_launch_kill))
}

async fn handle_launch_preflight(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> Result<Json<launch_app::LaunchPreflightResponse>, launch_app::LaunchApplicationError> {
    launch_app::prepare_launch_preflight(&state, instance_id)
        .await
        .map(Json)
}

async fn handle_launch(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<launch_app::LaunchRequest>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    let prepared = launch_app::prepare_launch_session_owned(&state, payload, &producer).await?;
    let response = launch_app::launch_prepared_response_payload(&prepared.task);
    spawn_launch_session(state, prepared.task, producer);

    Ok(Json(response))
}

fn spawn_launch_session(
    state: AppState,
    task: launch_app::LaunchSessionTask,
    producer: crate::state::ProducerLease,
) {
    let session_id = task.intent.session_id.clone();
    let launch_owner = producer.claim_child();
    producer.spawn(async move {
        if let Err(error) = launch_app::launch_session(state, task, launch_owner).await {
            tracing::warn!(
                session_id,
                error = %error.message,
                "background launch session ended with terminal error"
            );
        }
    });
}

async fn handle_benchmark_launch(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<launch_app::BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::launch_benchmark(state, payload, producer)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_launch(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<launch_app::BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::launch_benchmark_suite(state, payload, producer)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_tick(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<launch_app::BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::tick_benchmark_suite(state, payload, producer)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_driver_start(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<launch_app::BenchmarkLaunchRequest>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::start_benchmark_suite_driver(state, payload, producer)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_driver_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::benchmark_suite_driver_status(&state, &id)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_driver_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::benchmark_suite_driver_list(&state)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_driver_stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::stop_benchmark_suite_driver(&state, &id)
        .await
        .map(Json)
}

async fn handle_benchmark_suite_driver_resume(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::resume_benchmark_suite_driver(state, id, producer)
        .await
        .map(Json)
}

async fn handle_benchmark_matrix() -> Json<launch_app::BenchmarkMatrix> {
    Json(launch_app::benchmark_matrix())
}

async fn handle_benchmark_suite_manifest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::benchmark_suite_manifest(&state, &id).map(Json)
}

async fn handle_family_c_qualification(
    State(state): State<AppState>,
    Path(suite_id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::family_c_qualification(&state, &suite_id)
        .await
        .map(Json)
}

async fn handle_family_c_qualification_preview()
-> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::family_c_qualification_preview().map(Json)
}

async fn handle_launch_reports(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(launch_app::launch_reports_payload(&state))
}

async fn handle_launch_report(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::launch_report_payload(&state, &id).map(Json)
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
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::launch_command_payload(&state, &id)
        .await
        .map(Json)
}

async fn handle_launch_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    launch_app::launch_status_payload(&state, &id)
        .await
        .map(Json)
}

async fn handle_launch_kill(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, launch_app::LaunchApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(launch_app::launch_shutdown_error_response)?;
    launch_app::stop_launch_session(&state, &id, &producer)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests;
