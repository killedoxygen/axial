use super::error::{ApiResult, Extracted};
use axial_app::performance::{
    PerformanceService,
    benchmarks::{
        BenchmarkDriversResponse, BenchmarkError, BenchmarkLaunchRequest, BenchmarkMatrix,
        BenchmarkService, benchmark_matrix, qualification_preview,
    },
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    routing::{get, post},
};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
struct BenchmarkApi {
    benchmarks: Arc<BenchmarkService>,
    performance: Arc<PerformanceService>,
}

pub fn router(benchmarks: Arc<BenchmarkService>, performance: Arc<PerformanceService>) -> Router {
    Router::new()
        .route("/api/v1/launch/benchmark/suite/driver", post(start))
        .route("/api/v1/launch/benchmark/suite/drivers", get(drivers))
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/stop",
            post(stop),
        )
        .route(
            "/api/v1/launch/benchmark/suite/drivers/{id}/resume",
            post(resume),
        )
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

type Request = Result<Json<BenchmarkLaunchRequest>, JsonRejection>;

async fn matrix() -> Json<BenchmarkMatrix> {
    Json(benchmark_matrix())
}
async fn preview() -> Json<Value> {
    Json(qualification_preview())
}
async fn start(State(api): State<BenchmarkApi>, request: Request) -> ApiResult<Value> {
    let input = request.or_refuse(|| BenchmarkError::Invalid)?;
    let driver = api.benchmarks.start_driver(input).await?;
    Ok(Json(api.benchmarks.driver_view(driver)?))
}
async fn drivers(State(api): State<BenchmarkApi>) -> ApiResult<BenchmarkDriversResponse> {
    let drivers = api.benchmarks.driver_views(api.benchmarks.drivers()?)?;
    Ok(Json(BenchmarkDriversResponse {
        status: "ok",
        drivers,
    }))
}
async fn stop(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(
        api.benchmarks
            .driver_view(api.benchmarks.stop_driver(&id)?)?,
    ))
}
async fn resume(State(api): State<BenchmarkApi>, Path(id): Path<String>) -> ApiResult<Value> {
    Ok(Json(
        api.benchmarks
            .driver_view(api.benchmarks.resume_driver(&id)?)?,
    ))
}
async fn qualification(
    State(api): State<BenchmarkApi>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    Ok(Json(
        api.benchmarks.qualification(&id, &api.performance).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Profile;
    use axial_app::storage::StorageError;
    use reqwest::{Method, StatusCode};
    use serde_json::json;

    const DRIVERS: &str = "/api/v1/launch/benchmark/suite/drivers";

    #[tokio::test]
    async fn qualification_preview_is_descriptor_only_and_creates_no_work() {
        const PREVIEW: &str = "/api/v1/launch/benchmark/qualification/family-c-1-12-2/preview";
        let profile = Profile::start().await;
        let bootstrap = profile.services.server.bootstrap();
        let unauthorized = reqwest::Client::new()
            .get(format!("{}{PREVIEW}", bootstrap.base_url))
            .send()
            .await
            .unwrap()
            .status();
        let response = profile
            .api
            .builder(Method::GET, PREVIEW)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let data = response.text().await.unwrap();
        let drivers = profile.api.get(DRIVERS).await;
        let sessions = profile.api.get("/api/v1/launch/sessions").await;
        let services = &profile.services;
        services.server.shutdown().await.unwrap();
        services.server.wait().await.unwrap();
        let durable_counts = services
            .settings
            .metadata()
            .read::<_, StorageError>(|connection| {
                connection
                    .query_row(
                        "SELECT (SELECT count(*) FROM benchmark_suites),
                                (SELECT count(*) FROM launch_intents),
                                (SELECT count(*) FROM launch_reports)",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, u64>(0)?,
                                row.get::<_, u64>(1)?,
                                row.get::<_, u64>(2)?,
                            ))
                        },
                    )
                    .map_err(StorageError::from)
            })
            .unwrap();
        assert!(services.tasks.shutdown_receipt().is_some());
        assert_eq!(unauthorized, StatusCode::UNAUTHORIZED);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(drivers, json!({"status":"ok", "drivers":[]}));
        assert_eq!(sessions, json!({"sessions":[]}));
        assert_eq!(durable_counts, (0, 0, 0));
        assert!(data.len() < 4096);
        assert!(!data.contains('/') && !data.contains('\\'));
        assert!(!data.contains(&bootstrap.capability));
        let lower = data.to_ascii_lowercase();
        for forbidden in [
            "java_path",
            "command",
            "java-args",
            "account",
            "token",
            "runtime",
            "launch_intent",
        ] {
            assert!(!lower.contains(forbidden), "preview exposed {forbidden}");
        }
        let payload: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            payload["schema"],
            "axial.launch.benchmark.qualification.family_c_1_12_2"
        );
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["status"], "incomplete");
        assert_eq!(
            payload["suite"],
            json!({
                "present":false, "mode":"release_validation", "run_count":8,
            })
        );
        assert_eq!(
            payload["target"],
            json!({
                "family":"C", "loader":"Forge", "version":"1.12.2", "mode":"release_validation",
            })
        );
        assert_eq!(
            payload["view_model"],
            json!({
                "status_label":"Incomplete", "status_tone":"warn",
                "target_label":"Family C, Forge, 1.12.2, Release Validation",
                "suite_label":"Suite missing", "schema_label":"v1",
                "missing_summary":"7 missing: Proof Missing, Suite Manifest Missing, +5",
                "suite_summary":"Suite missing",
                "evidence_summary":"Baseline: Pending, run #1, Proof missing | Managed: Pending, run #2, Proof missing",
            })
        );
        let targets = payload["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0]["suite_run"],
            json!({
                "present":true, "run_index":0, "profile":"vanilla_baseline", "run_type":"coldish",
                "target_id":"family_c_forge_1_12_2_vanilla_baseline",
                "benchmark_id":"benchmark-cb555607e50285e6", "session_id":null, "state":"pending",
            })
        );
        assert_eq!(
            targets[1]["suite_run"],
            json!({
                "present":true, "run_index":1, "profile":"managed_default", "run_type":"coldish",
                "target_id":"family_c_forge_1_12_2_family_c_forge_core",
                "benchmark_id":"benchmark-83e6d2fae9587b9c", "session_id":null, "state":"pending",
            })
        );
        assert_eq!(
            targets[0]["missing"],
            json!([
                "proof_missing",
                "suite_manifest_missing",
                "suite_run_session_missing",
            ])
        );
        assert_eq!(
            targets[1]["missing"],
            json!([
                "managed_comparison_missing",
                "proof_missing",
                "suite_manifest_missing",
                "suite_run_session_missing",
            ])
        );
        for target in targets {
            assert_eq!(target["proof"], json!({"present":false}));
        }
    }

    #[tokio::test]
    async fn current_driver_routes_resume_the_same_driver_and_project_actions() {
        let profile = Profile::start().await;
        let services = &profile.services;
        let (instance, _) = profile.create_instance().await;
        let accepted = services
            .benchmarks
            .start_driver(
                serde_json::from_value(json!({
                    "instance_id": instance,
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
        let path = format!("{DRIVERS}/{}", accepted.id);
        let bootstrap = services.server.bootstrap();
        assert_eq!(
            reqwest::Client::new()
                .get(format!("{}{DRIVERS}", bootstrap.base_url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let stopped = profile.api.driver(&path).await;
        assert_eq!(stopped["driver"]["id"], accepted.id);
        assert_eq!(stopped["view_model"]["state_label"], "stopped");
        assert_eq!(stopped["view_model"]["state_tone"], "warn");
        assert_eq!(stopped["view_model"]["can_stop"], false);
        assert_eq!(stopped["view_model"]["can_resume"], true);
        let resumed = profile
            .api
            .post(&format!("{path}/resume"), Value::Null)
            .await;
        assert_eq!(resumed["driver"]["id"], accepted.id);
        assert_eq!(resumed["driver"]["suite_id"], accepted.suite_id);
        assert_eq!(resumed["driver"]["state"], "running");
        assert_eq!(resumed["view_model"]["can_stop"], true);
        assert_eq!(resumed["view_model"]["can_resume"], false);
        let stopped = profile.api.post(&format!("{path}/stop"), Value::Null).await;
        assert_eq!(stopped["driver"]["id"], accepted.id);
        assert_eq!(stopped["driver"]["state"], "stopped");
        assert_eq!(stopped["view_model"]["can_stop"], false);
        assert_eq!(stopped["view_model"]["can_resume"], true);
        let listed = profile.api.get(DRIVERS).await;
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
