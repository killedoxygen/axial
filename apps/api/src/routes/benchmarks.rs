use axial_app::performance::{
    PerformanceService,
    benchmarks::{
        BenchmarkError, BenchmarkLaunchRequest, BenchmarkService, BenchmarkSuiteDriverStatus,
        benchmark_matrix, driver_payload, qualification_preview,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct BenchmarkApi {
    benchmarks: Arc<BenchmarkService>,
    performance: Arc<PerformanceService>,
}
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn router(benchmarks: Arc<BenchmarkService>, performance: Arc<PerformanceService>) -> Router {
    Router::new()
        .route("/api/v1/launch/benchmark", post(launch))
        .route("/api/v1/launch/benchmark/suite", post(tick))
        .route("/api/v1/launch/benchmark/suite/tick", post(tick))
        .route("/api/v1/launch/benchmark/suite/driver", post(start))
        .route("/api/v1/launch/benchmark/suite/drivers", get(drivers))
        .route("/api/v1/launch/benchmark/suite/drivers/{id}", get(driver))
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/stop",
            post(stop),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/resume",
            post(resume),
        )
        .route("/api/v1/launch/benchmark/suites/{id}", get(suite))
        .route("/api/v1/launch/benchmark/matrix", get(matrix))
        .route(
            "/api/v1/launch/benchmark/qualification/family-c-1-12-2/preview",
            get(preview),
        )
        .route(
            "/api/v1/launch/benchmark/qualification/family-c-1-12-2/{id}",
            get(qualification),
        )
        .layer(DefaultBodyLimit::max(8192))
        .with_state(BenchmarkApi {
            benchmarks,
            performance,
        })
}
async fn matrix() -> Json<Value> {
    Json(json!(benchmark_matrix()))
}
async fn preview() -> Json<Value> {
    Json(qualification_preview())
}
async fn launch(
    State(api): State<BenchmarkApi>,
    request: Result<Json<BenchmarkLaunchRequest>, JsonRejection>,
) -> ApiResult {
    let Json(input) = request.map_err(|_| error(BenchmarkError::Invalid))?;
    Ok(Json(json!(
        api.benchmarks.launch(input).await.map_err(error)?
    )))
}
async fn tick(
    State(api): State<BenchmarkApi>,
    request: Result<Json<BenchmarkLaunchRequest>, JsonRejection>,
) -> ApiResult {
    let Json(input) = request.map_err(|_| error(BenchmarkError::Invalid))?;
    api.benchmarks.tick(input).await.map(Json).map_err(error)
}
async fn start(
    State(api): State<BenchmarkApi>,
    request: Result<Json<BenchmarkLaunchRequest>, JsonRejection>,
) -> ApiResult {
    let Json(input) = request.map_err(|_| error(BenchmarkError::Invalid))?;
    let driver = api.benchmarks.start_driver(input).await.map_err(error)?;
    project_driver(&api.benchmarks, driver)
}
async fn drivers(State(api): State<BenchmarkApi>) -> ApiResult {
    let drivers = api.benchmarks.drivers().map_err(error)?;
    let mut actions = api.benchmarks.resume_actions(&drivers).map_err(error)?;
    let drivers = drivers
        .into_iter()
        .map(|driver| {
            let action = actions
                .remove(&driver.id)
                .ok_or_else(|| error(BenchmarkError::Unavailable))?;
            Ok(driver_response(driver, action))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({"status":"ok","drivers":drivers})))
}
async fn driver(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult {
    project_driver(&api.benchmarks, api.benchmarks.driver(&id).map_err(error)?)
}
async fn stop(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult {
    project_driver(
        &api.benchmarks,
        api.benchmarks.stop_driver(&id).map_err(error)?,
    )
}
async fn resume(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult {
    project_driver(
        &api.benchmarks,
        api.benchmarks.resume_driver(&id).map_err(error)?,
    )
}
fn project_driver(service: &BenchmarkService, driver: BenchmarkSuiteDriverStatus) -> ApiResult {
    let action = service
        .resume_actions(std::slice::from_ref(&driver))
        .map_err(error)?
        .remove(&driver.id)
        .ok_or_else(|| error(BenchmarkError::Unavailable))?;
    Ok(Json(driver_response(driver, action)))
}
fn driver_response(driver: BenchmarkSuiteDriverStatus, can_resume: bool) -> Value {
    let mut payload = driver_payload(driver);
    payload["view_model"]["can_resume"] = json!(can_resume);
    payload
}
async fn suite(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(
        api.benchmarks
            .suite(&id)
            .map_err(error)?
            .ok_or_else(|| error(BenchmarkError::NotFound))?
    )))
}
async fn qualification(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult {
    api.benchmarks
        .qualification(&id, &api.performance)
        .await
        .map(Json)
        .map_err(error)
}
fn error(error: BenchmarkError) -> (StatusCode, Json<Value>) {
    let status = match error {
        BenchmarkError::Invalid => StatusCode::BAD_REQUEST,
        BenchmarkError::NotFound => StatusCode::NOT_FOUND,
        BenchmarkError::Busy | BenchmarkError::Complete => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":error.to_string()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        instances::create::{CreateInstanceRequest, CreateTarget},
        storage::StorageError,
    };
    use reqwest::Method;

    const DRIVERS: &str = "/api/v1/launch/benchmark/suite/drivers";

    #[tokio::test]
    async fn current_driver_routes_resume_the_same_driver_and_project_actions() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let services = crate::start_in_profile(root.path().join("replacement"), None)
            .await
            .unwrap();
        let target = CreateTarget::loader_for_tests(
            axial_minecraft::LoaderComponentId::Fabric,
            "1.21.1",
            "0.16.9",
        )
        .unwrap();
        let instance = services
            .instances
            .create(
                CreateInstanceRequest {
                    name: "Benchmark route fixture".into(),
                    selection_id: target.selection_id().into(),
                    ..Default::default()
                },
                target,
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let accepted = services
            .benchmarks
            .start_driver(
                serde_json::from_value(json!({
                    "instance_id": instance.id,
                    "suite_mode": "development",
                    "interval_ms": 5_000
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        services.benchmarks.stop_driver(&accepted.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !services.tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let storage = services.settings.metadata();
        let captured_request = || {
            storage
                .read::<_, StorageError>(|connection| {
                    Ok(connection.query_row(
                        "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                        [&accepted.id],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?)
                })
                .unwrap()
        };
        let original_request = captured_request();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let request = |method, path: &str| {
            client
                .request(method, format!("{}{path}", bootstrap.base_url))
                .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
        };
        let path = format!("{DRIVERS}/{}", accepted.id);
        assert_eq!(
            client
                .get(format!("{}{path}", bootstrap.base_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let stopped: Value = request(Method::GET, &path)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stopped["driver"]["id"], accepted.id);
        assert_eq!(stopped["view_model"]["state_label"], "stopped");
        assert_eq!(stopped["view_model"]["state_tone"], "warn");
        assert_eq!(stopped["view_model"]["can_stop"], false);
        assert_eq!(stopped["view_model"]["can_resume"], true);
        let resumed: Value = request(Method::POST, &format!("{path}/resume"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resumed["driver"]["id"], accepted.id);
        assert_eq!(resumed["driver"]["suite_id"], accepted.suite_id);
        assert_eq!(resumed["driver"]["state"], "running");
        assert_eq!(resumed["view_model"]["can_stop"], true);
        assert_eq!(resumed["view_model"]["can_resume"], false);
        let stopped: Value = request(Method::POST, &format!("{path}/stop"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stopped["driver"]["id"], accepted.id);
        assert_eq!(stopped["driver"]["state"], "stopped");
        assert_eq!(stopped["view_model"]["can_stop"], false);
        assert_eq!(stopped["view_model"]["can_resume"], true);
        let listed: Value = request(Method::GET, DRIVERS)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let drivers = listed["drivers"].as_array().unwrap();
        assert_eq!(drivers.len(), 1);
        assert_eq!(drivers[0]["driver"]["id"], accepted.id);
        assert_eq!(drivers[0]["view_model"]["can_resume"], true);
        assert_eq!(captured_request(), original_request);
        storage
            .read::<_, StorageError>(|connection| {
                assert_eq!(
                    connection.query_row("SELECT count(*) FROM benchmark_suites", [], |row| row
                        .get::<_, usize>(0))?,
                    1
                );
                assert_eq!(
                    connection
                        .query_row("SELECT count(*) FROM benchmark_drivers", [], |row| row
                            .get::<_, usize>(0))?,
                    1
                );
                Ok(())
            })
            .unwrap();
        assert!(services.sessions.snapshots().is_empty());
        services.server.shutdown().await.unwrap();
        services.server.wait().await.unwrap();
    }
}
