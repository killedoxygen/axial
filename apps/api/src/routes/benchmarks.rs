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
fn driver_response(
    driver: BenchmarkSuiteDriverStatus,
    (can_resume, resumed): (bool, Option<String>),
) -> Value {
    let mut payload = driver_payload(driver);
    payload["view_model"]["can_resume"] = json!(can_resume);
    if let Some(resumed) = resumed {
        payload["resumed_driver_id"] = json!(resumed);
    }
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
        BenchmarkError::Busy | BenchmarkError::Complete | BenchmarkError::ConflictingHistory => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":error.to_string()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::{
        performance::benchmarks::{benchmark_suite_plan, benchmark_suite_run_id},
        storage::{MetadataStore, StorageError, rusqlite::params},
    };
    use reqwest::Method;

    const DRIVERS: &str = "/api/v1/launch/benchmark/suite/drivers";

    fn history(key: &str) -> (Value, Value) {
        let suite_id = format!("legacy-suite-{}", key.repeat(64));
        let runs: Vec<_> = benchmark_suite_plan("development")
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, run)| {
                json!({"run_index":index,"profile":run.profile,"run_type":run.run_type,
                    "target_id":run.target_id.unwrap_or(""),
                    "benchmark_id":benchmark_suite_run_id("development", index, run),
                    "session_id":null,"launched_at":null,"state":"pending"})
            })
            .collect();
        let suite = json!({
            "schema":"axial.launch.benchmark.suite","schema_version":2,
            "suite_id":suite_id,"instance_id":"12345678-1234-4234-8234-123456789abc",
            "mode":"development","created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:02Z","runs":runs,"historical":true
        });
        let driver = json!({
            "id":format!("legacy-driver-{}", key.repeat(64)),"suite_id":suite_id,
            "mode":"development","state":"stopped","interval_ms":30000,
            "run_count":runs.len(),"launched_run_count":0,"pending_run_index":0,
            "active_session_id":null,"last_run_index":null,"last_session_id":null,
            "error":null,"created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:02Z","historical":true
        });
        (suite, driver)
    }

    fn stored_rows(
        storage: &MetadataStore,
    ) -> Vec<(String, Vec<u8>, Option<Vec<u8>>, Option<String>)> {
        storage
            .read::<_, StorageError>(|connection| {
                let mut query = connection.prepare(
                    "SELECT suite_id,payload,NULL,source_suite_id FROM benchmark_suites
                     UNION ALL SELECT driver_id,payload,request,source_driver_id FROM benchmark_drivers
                     ORDER BY 1",
                )?;
                Ok(query
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn historical_driver_routes_reconcile_exact_successor_without_mutating_evidence() {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let services = crate::start_in_profile(root.path().join("replacement"), None)
            .await
            .unwrap();
        let storage = services.settings.metadata();
        let (source_suite, source_driver) = history("a");
        let (mut unsupported_suite, unsupported_driver) = history("b");
        unsupported_suite["runs"][0]["profile"] = json!("retained_old_profile");
        storage
            .transaction::<_, StorageError>(|tx| {
                for (suite, driver) in [
                    (&source_suite, &source_driver),
                    (&unsupported_suite, &unsupported_driver),
                ] {
                    tx.execute(
                        "INSERT INTO benchmark_suites(suite_id,payload) VALUES(?1,?2)",
                        params![
                            suite["suite_id"].as_str().unwrap(),
                            serde_json::to_vec(suite).unwrap()
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO benchmark_drivers(driver_id,payload) VALUES(?1,?2)",
                        params![
                            driver["id"].as_str().unwrap(),
                            serde_json::to_vec(driver).unwrap()
                        ],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let bootstrap = services.server.bootstrap();
        let client = reqwest::Client::new();
        let request = |method, path: &str| {
            client
                .request(method, format!("{}{path}", bootstrap.base_url))
                .header(crate::transport::CAPABILITY_HEADER, &bootstrap.capability)
        };
        let source_id = source_driver["id"].as_str().unwrap();
        let unsupported_id = unsupported_driver["id"].as_str().unwrap();
        let before = stored_rows(storage);
        assert_eq!(
            client
                .get(format!("{}{DRIVERS}/{source_id}", bootstrap.base_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        for (id, expected) in [
            (source_id, &source_driver),
            (unsupported_id, &unsupported_driver),
        ] {
            let detail: Value = request(Method::GET, &format!("{DRIVERS}/{id}"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(&detail["driver"], expected);
            assert_eq!(
                detail["view_model"]["state_label"],
                "Historical stopped (read-only)"
            );
            assert_eq!(detail["view_model"]["state_tone"], "warn");
            assert_eq!(detail["view_model"]["can_stop"], false);
            assert_eq!(detail["view_model"]["can_resume"], false);
            assert!(detail.get("resumed_driver_id").is_none());
        }
        for action in ["resume", "stop"] {
            assert_eq!(
                request(
                    Method::POST,
                    &format!("{DRIVERS}/{unsupported_id}/{action}")
                )
                .send()
                .await
                .unwrap()
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(stored_rows(storage), before);

        // Recreate an already accepted, stopped successor with exact owner proof.
        // Acceptance and real execution are covered by the composed import journey.
        let mut successor_suite = source_suite.clone();
        successor_suite
            .as_object_mut()
            .unwrap()
            .remove("historical");
        successor_suite["suite_id"] = json!("suite-11111111111141118111111111111111");
        for run in successor_suite["runs"].as_array_mut().unwrap() {
            run["launch_intent"] = json!(uuid::Uuid::new_v4().to_string());
        }
        let mut successor_driver = source_driver.clone();
        successor_driver
            .as_object_mut()
            .unwrap()
            .remove("historical");
        successor_driver["id"] = json!("benchmark-suite-driver-22222222222242228222222222222222");
        successor_driver["suite_id"] = successor_suite["suite_id"].clone();
        let captured = json!({"instance_id":source_suite["instance_id"],
            "suite_id":successor_suite["suite_id"],"suite_mode":"development","interval_ms":30000});
        storage.transaction::<_, StorageError>(|tx| {
            tx.execute("INSERT INTO benchmark_suites(suite_id,payload,source_suite_id) VALUES(?1,?2,?3)",
                params![successor_suite["suite_id"].as_str().unwrap(), serde_json::to_vec(&successor_suite).unwrap(), source_suite["suite_id"].as_str().unwrap()])?;
            tx.execute("INSERT INTO benchmark_drivers(driver_id,payload,request,source_driver_id) VALUES(?1,?2,?3,?4)",
                params![successor_driver["id"].as_str().unwrap(), serde_json::to_vec(&successor_driver).unwrap(), serde_json::to_vec(&captured).unwrap(), source_id])?;
            Ok(())
        }).unwrap();
        let successor_id = successor_driver["id"].as_str().unwrap();
        assert_eq!(
            services
                .benchmarks
                .resumed_driver(source_id)
                .unwrap()
                .unwrap()
                .id,
            successor_id
        );
        let accepted = stored_rows(storage);
        let source: Value = request(Method::GET, &format!("{DRIVERS}/{source_id}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(source["driver"], source_driver);
        assert_eq!(source["resumed_driver_id"], successor_id);
        assert_eq!(source["view_model"]["can_resume"], false);
        assert_eq!(
            source["view_model"]["state_label"],
            "Historical stopped (read-only)"
        );
        let successor: Value = request(Method::GET, &format!("{DRIVERS}/{successor_id}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(successor["driver"], successor_driver);
        assert_eq!(successor["view_model"]["state_label"], "stopped");
        assert_eq!(successor["view_model"]["can_resume"], true);
        assert!(successor["driver"].get("historical").is_none());
        assert!(successor.get("resumed_driver_id").is_none());
        for _ in 0..2 {
            let replay: Value = request(Method::POST, &format!("{DRIVERS}/{source_id}/resume"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(replay, successor);
        }
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
        assert_eq!(drivers.len(), 3);
        assert_eq!(
            drivers
                .iter()
                .find(|driver| driver["driver"]["id"] == source_id)
                .unwrap(),
            &source
        );
        assert_eq!(
            drivers
                .iter()
                .find(|driver| driver["driver"]["id"] == successor_id)
                .unwrap(),
            &successor
        );
        assert_eq!(stored_rows(storage), accepted);
        assert!(services.sessions.snapshots().is_empty());
        storage
            .read::<_, StorageError>(|connection| {
                assert_eq!(
                    connection
                        .query_row("SELECT COUNT(*) FROM launch_intents", [], |row| row
                            .get::<_, u64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
        services.server.shutdown().await.unwrap();
        services.server.wait().await.unwrap();
    }
}
