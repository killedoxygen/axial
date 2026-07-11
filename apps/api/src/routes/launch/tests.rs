use super::*;
use crate::application::launch::*;
use crate::application::performance::{
    FAMILY_C_BASELINE_TARGET_ID, FAMILY_C_MANAGED_COMPOSITION_ID, FAMILY_C_MANAGED_TARGET_ID,
    FAMILY_C_QUALIFICATION_VERSION, benchmark_suite_manifest_run_inputs, benchmark_suite_plan,
    benchmark_suite_run_id, family_c_qualification_payload, family_c_qualification_preview_payload,
};
use crate::execution::file::{FileWriteRequest, write_file_atomically};
use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
use crate::state::contracts::TargetDescriptor;
use crate::state::{AppStateInit, InstallStore, SessionStore};
use axial_config::{AppConfig, AppPaths, ConfigStore, Instance, InstanceStore};
use axial_launcher::{
    GuardianDecision, GuardianMode, GuardianSummary, LaunchAuthContext, LaunchGuardianContext,
    LaunchIntent, LaunchSessionRecord, LaunchStageRecord, LaunchState, SessionId,
};
use axial_performance::PerformanceManager;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
fn launch_success_response_payload_exposes_effective_memory() {
    let payload = launch_app::launch_success_response_payload(&launch_app::LaunchSuccess {
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
fn launch_prepared_response_payload_exposes_queued_session_metadata() {
    let task = test_launch_session_task();
    let payload = launch_app::launch_prepared_response_payload(&task);

    assert_eq!(payload["status"], serde_json::json!("launching"));
    assert_eq!(payload["state"], serde_json::json!("queued"));
    assert_eq!(payload["session_id"], serde_json::json!("session-queued"));
    assert_eq!(payload["instance_id"], serde_json::json!("instance-queued"));
    assert_eq!(payload["pid"], serde_json::Value::Null);
    assert_eq!(
        payload["launched_at"],
        serde_json::json!("2026-05-30T00:00:00Z")
    );
    assert_eq!(payload["max_memory_mb"], serde_json::json!(6144));
    assert_eq!(payload["min_memory_mb"], serde_json::json!(1024));
    assert_eq!(payload["healing"], serde_json::Value::Null);
    assert_eq!(
        payload["guardian"]["decision"],
        serde_json::json!("allowed")
    );
}

#[test]
fn launch_request_error_status_maps_guardian_blocked_to_unprocessable_entity() {
    let error = launch_request_error(Some(GuardianDecision::Blocked));

    assert_eq!(
        launch_request_error_status(&error),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let response = launch_request_error_response(error);
    assert_eq!(response.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response.1.0["error"], serde_json::json!("launch rejected"));
    assert_eq!(
        response.1.0["guardian"]["decision"],
        serde_json::json!("blocked")
    );
}

#[test]
fn launch_request_error_status_keeps_non_guardian_blocked_errors_internal() {
    for error in [
        launch_request_error(None),
        launch_request_error(Some(GuardianDecision::Warned)),
    ] {
        assert_eq!(
            launch_request_error_status(&error),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            launch_request_error_response(error).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

#[test]
fn launch_request_error_response_sanitizes_public_error_payload() {
    let response = launch_request_error_response(launch_app::LaunchRequestError {
            message: "prepare failed for /home/alice/.axial --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer\njava.exe C:\\Users\\Alice\\AppData"
                .to_string(),
            healing: None,
            guardian: None,
        });

    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    let message = response.1.0["error"].as_str().expect("error message");
    assert!(message.contains("Launch failed before Minecraft could start"));
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
    ] {
        assert!(
            !message.contains(fragment),
            "launch error response leaked fragment {fragment:?}: {message}"
        );
    }
}

#[test]
fn launch_kill_not_found_error_response_uses_session_not_found() {
    let response = launch_kill_error_response(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "failed to kill process 4242 at /home/alice/.axial/secret",
    ));

    assert_eq!(response.0, StatusCode::NOT_FOUND);
    assert_eq!(
        response.1.0,
        serde_json::json!({ "error": "session not found" })
    );
}

#[test]
fn launch_kill_internal_error_response_hides_raw_io_details() {
    let response = launch_kill_error_response(raw_launch_control_io_error());

    assert_public_error_excludes_raw_launch_control_fragments(
        response,
        StatusCode::INTERNAL_SERVER_ERROR,
        LAUNCH_KILL_INTERNAL_ERROR_MESSAGE,
    );
}

#[test]
fn benchmark_suite_storage_error_response_hides_raw_io_details() {
    let response = benchmark_suite_store_error_response(
        crate::state::benchmark_suites::BenchmarkSuiteStoreError::Persistence(
            raw_benchmark_suite_storage_io_error(),
        ),
    );

    assert_eq!(response.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.1.0,
        serde_json::json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })
    );
    let data = serde_json::to_string(&response.1.0).expect("serialize public error");
    for fragment in [
        "/home/alice",
        ".axial",
        "C:\\Users\\Alice",
        "AppData",
        "Permission denied",
        "os error 13",
        "family-c-1-12-2",
        "suite-release_validation-00-family_c_forge",
        "benchmark-suites",
        "family-c-1-12-2.json",
    ] {
        assert!(
            !data.contains(fragment),
            "benchmark suite storage error leaked fragment {fragment:?}: {data}"
        );
    }
}

#[test]
fn launch_command_sanitizer_redacts_entire_command_shape() {
    let command = vec![
        "java".to_string(),
        "-Xmx4096M".to_string(),
        "--accessToken".to_string(),
        "online-secret-token".to_string(),
        "--access_token=snake-secret-token".to_string(),
        "--auth_access_token".to_string(),
        "auth-secret-token".to_string(),
        "-Dauth.accessToken=jvm-secret-token".to_string(),
        "-Dcustom.auth_token=property-secret-token".to_string(),
        "--device_code".to_string(),
        "device-secret-code".to_string(),
        "--provider_payload=provider-secret-payload".to_string(),
        "com.mojang.Main".to_string(),
        "--username".to_string(),
        "Player".to_string(),
    ];

    let sanitized = sanitize_launch_command(&command);
    let data = serde_json::to_string(&sanitized.command).expect("serialize command");

    assert!(sanitized.redacted);
    assert_eq!(sanitized.command.len(), command.len());
    assert!(
        sanitized
            .command
            .iter()
            .all(|arg| arg == LAUNCH_COMMAND_REDACTED_VALUE)
    );
    assert!(!data.contains("java"));
    assert!(!data.contains("-Xmx4096M"));
    assert!(!data.contains("com.mojang.Main"));
    assert!(!data.contains("--username"));
    assert!(!data.contains("Player"));
    assert!(!data.contains("online-secret-token"));
    assert!(!data.contains("snake-secret-token"));
    assert!(!data.contains("auth-secret-token"));
    assert!(!data.contains("jvm-secret-token"));
    assert!(!data.contains("property-secret-token"));
    assert!(!data.contains("device-secret-code"));
    assert!(!data.contains("provider-secret-payload"));
}

#[test]
fn launch_command_sanitizer_leaves_empty_command_unredacted() {
    let sanitized = sanitize_launch_command(&[]);
    assert!(!sanitized.redacted);
    assert!(sanitized.command.is_empty());
}

#[tokio::test]
async fn launch_command_route_redacts_public_diagnostics_from_json() {
    let fixture = RouteTestFixture::new("launch-command-redacts");
    let raw_access_token = "online-access-token-secret";
    let mut record = test_record("command-sensitive");
    record.command = vec![
        "java".to_string(),
        "-Xmx2048M".to_string(),
        "--accessToken".to_string(),
        raw_access_token.to_string(),
        "--access_token=snake-secret-token".to_string(),
        "-Dauth.accessToken=jvm-secret-token".to_string(),
        "net.minecraft.client.main.Main".to_string(),
    ];
    record.java_path = Some("/home/SecretUser/bin/java".to_string());
    record.guardian = Some(json!({
        "decision": "blocked",
        "message": "Guardian blocked unsafe launch setup.",
        "details": [
            "safe fallback applied",
            "custom Java path C:\\Users\\Alice\\AppData\\Local\\java.exe"
        ],
        "provider_payload": { "token": "guardian-secret" }
    }));
    record.healing = Some(json!({
        "fallback_applied": "managed_java",
        "warnings": [
            "safe warning",
            "-Xmx8192M was removed from C:\\Users\\Alice\\AppData\\Local\\java.exe"
        ],
        "account_id": "account-secret"
    }));
    fixture
        .state
        .sessions()
        .insert(record)
        .await
        .expect("insert session");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/launch/command-sensitive/command")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("command json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert_eq!(
        payload["session_id"],
        serde_json::json!("command-sensitive")
    );
    assert_eq!(payload["command_redacted"], serde_json::json!(true));
    assert_eq!(payload["command_arg_count"], serde_json::json!(7));
    assert_eq!(payload["java_path_present"], serde_json::json!(true));
    assert_eq!(payload["command"][0], serde_json::json!("<redacted>"));
    assert_eq!(payload["command"][1], serde_json::json!("<redacted>"));
    assert_eq!(
        payload["guardian"]["message"],
        serde_json::json!("Guardian blocked unsafe launch setup.")
    );
    assert_eq!(
        payload["healing"]["fallback_applied"],
        serde_json::json!("managed_java")
    );
    assert!(data.contains("safe fallback applied"));
    assert!(data.contains("safe warning"));
    assert!(!data.contains("net.minecraft.client.main.Main"));
    assert!(!data.contains("-Xmx2048M"));
    assert!(!data.contains("--accessToken"));
    assert!(!data.contains(raw_access_token));
    assert!(!data.contains("snake-secret-token"));
    assert!(!data.contains("jvm-secret-token"));
    assert!(!data.contains("SecretUser"));
    assert!(!data.contains("Alice"));
    assert!(!data.contains("AppData"));
    assert!(!data.contains("guardian-secret"));
    assert!(!data.contains("account-secret"));
    assert!(!data.contains("provider_payload"));
    assert!(!data.contains("account_id"));
    assert!(!data.contains("java_path\""));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_status_route_redacts_raw_record_diagnostics_from_json() {
    let fixture = RouteTestFixture::new("launch-status-redacts");
    let mut record = test_record("status-sensitive");
    record.guardian = Some(json!({
        "decision": "warned",
        "message": "Guardian applied managed defaults.",
        "details": [
            "safe detail",
            "custom Java path /home/alice/.minecraft/java.exe"
        ],
        "provider_payload": { "token": "guardian-secret" }
    }));
    record.healing = Some(json!({
        "fallback_applied": "managed_java",
        "warnings": [
            "safe warning",
            "removed -Xmx8192M from C:\\Users\\Alice\\AppData\\Local\\java.exe"
        ],
        "account_id": "account-secret"
    }));
    record.benchmark = Some(json!({
        "health": "ok",
        "raw": "/tmp/secret-fallback"
    }));
    fixture
        .state
        .sessions()
        .insert(record)
        .await
        .expect("insert session");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/launch/status-sensitive/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert!(data.contains("Guardian applied managed defaults."));
    assert!(data.contains("safe detail"));
    assert!(data.contains("safe warning"));
    assert!(data.contains("managed_java"));
    for fragment in [
        "/home/alice",
        ".minecraft",
        "java.exe",
        "-Xmx8192M",
        "C:\\\\Users",
        "AppData",
        "/tmp/secret-fallback",
        "provider_payload",
        "guardian-secret",
        "account_id",
        "account-secret",
    ] {
        assert!(
            !data.contains(fragment),
            "launch status leaked fragment {fragment:?}: {data}"
        );
    }

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_reports_route_exports_sanitized_proof_records() {
    let fixture = RouteTestFixture::new("launch-reports-sanitized-list");
    write_family_c_proof_record(&fixture, &sensitive_launch_proof_record());

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/launch/reports")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("reports json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");
    let report = &payload["reports"][0];

    assert_sanitized_launch_proof_payload(report);
    assert_launch_proof_payload_excludes_sensitive_content(&data);

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_report_route_exports_sanitized_proof_record() {
    let fixture = RouteTestFixture::new("launch-reports-sanitized-detail");
    write_family_c_proof_record(&fixture, &sensitive_launch_proof_record());

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/launch/reports/sensitive-proof")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("report json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert_sanitized_launch_proof_payload(&payload);
    assert_launch_proof_payload_excludes_sensitive_content(&data);

    cleanup(&fixture.root);
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
    assert_eq!(input.requested_run_index, None);
    assert_eq!(input.plan.len(), 2);
    assert_eq!(input.plan[0].profile, "vanilla_baseline");
    assert_eq!(input.plan[0].run_type, "coldish");
}

#[test]
fn benchmark_suite_request_preserves_explicit_run_index_for_store_selection() {
    let suite_id = test_suite_id("explicit-run-index", "development");
    let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
        "instance_id": "instance",
        "suite_mode": "development",
        "suite_id": &suite_id,
        "run_index": 0
    }))
    .expect("deserialize suite request");

    let input = request
        .into_suite_launch_input()
        .expect("suite input should parse");

    assert_eq!(input.requested_run_index, Some(0));
}

#[tokio::test]
async fn benchmark_suite_run_reservation_records_prepared_session_before_launch_execution() {
    let store = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
    let suite_id = test_suite_id("run-reservation", "development");
    let plan = benchmark_suite_plan("development").expect("development plan");
    let input = BenchmarkSuiteLaunchInput {
        launch: launch_app::LaunchRequest {
            instance_id: "instance".to_string(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
        suite_id,
        mode: "development".to_string(),
        requested_run_index: Some(1),
        plan,
    };

    let manifest_runs = benchmark_suite_manifest_run_inputs(&input.mode, &input.plan);
    let manifest = persist_suite_run(
        &store,
        &input.suite_id,
        "instance",
        &input.mode,
        &manifest_runs,
        input.requested_run_index.expect("explicit run index"),
        "session-prepared-before-spawn",
        "2026-01-01T00:00:00.000Z",
    )
    .await
    .expect("reservation should persist");

    let selected = manifest
        .runs
        .iter()
        .find(|run| run.run_index == 1)
        .expect("selected run");
    assert_eq!(
        selected.session_id.as_deref(),
        Some("session-prepared-before-spawn")
    );
    let launched_at = chrono::DateTime::parse_from_rfc3339(
        selected.launched_at.as_deref().expect("launch timestamp"),
    )
    .expect("stored launch timestamp");
    let expected_launched_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
        .expect("expected launch timestamp");
    assert_eq!(launched_at, expected_launched_at);
    assert_eq!(selected.state, "launching");

    let pending = manifest
        .runs
        .iter()
        .find(|run| run.run_index == 0)
        .expect("pending run");
    assert_eq!(pending.session_id, None);
    assert_eq!(pending.state, "pending");
}

#[tokio::test]
async fn benchmark_suite_run_reservation_storage_error_is_bounded() {
    let root = test_root("suite-run-reservation-error");
    let paths = test_paths(&root);
    let backend = Arc::new(FailOnceBenchmarkSuiteBackend::new());
    let coordinator = PersistenceCoordinator::for_test(backend, Duration::ZERO, Duration::ZERO);
    let store =
        crate::state::benchmark_suites::BenchmarkSuiteStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator,
        )
        .expect("load injected suite store");
    let plan = benchmark_suite_plan("development").expect("development plan");
    let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
    let suite_id = test_suite_id("bounded-storage-error", "development");
    let selection = store
        .select_reservation(
            &suite_id,
            "instance",
            "development",
            &manifest_runs,
            Some(0),
        )
        .await
        .expect("select suite reservation");
    let reserve_error = store
        .reserve(
            selection,
            "session-bounded-storage-error",
            "2026-01-01T00:00:00.000Z",
            false,
        )
        .await
        .expect_err("injected storage write should fail");
    let (handle, source) = match reserve_error {
        crate::state::benchmark_suites::BenchmarkSuiteReserveError::AcceptedWriteFailed {
            handle,
            source,
        } => (handle, source),
        crate::state::benchmark_suites::BenchmarkSuiteReserveError::PreAccept(error) => {
            panic!("reservation failed before acceptance: {}", error.class())
        }
    };
    let error = benchmark_suite_store_error_response(
        crate::state::benchmark_suites::BenchmarkSuiteStoreError::Persistence(source),
    );

    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        error.1.0,
        serde_json::json!({ "error": BENCHMARK_SUITE_STORAGE_ERROR_MESSAGE })
    );
    let body = serde_json::to_string(&error.1.0).expect("error json");
    assert!(!body.contains(root.to_string_lossy().as_ref()));
    assert!(!body.contains("Alice"));
    assert!(!body.contains("suite.json"));

    store
        .settle_compensation(&handle)
        .await
        .expect("settle exact compensation");
    store.close().await.expect("close injected suite store");

    cleanup(&root);
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
    let plan = benchmark_suite_plan("development").expect("development plan");
    let suite_id = test_suite_id("response-payload", "development");
    let payload = benchmark_suite_status_payload(&suite_id, "development", 0, &plan);
    let selected_benchmark_id = benchmark_suite_run_id("development", 0, plan[0]);
    let remaining_benchmark_id = benchmark_suite_run_id("development", 1, plan[1]);

    assert_eq!(
        payload,
        serde_json::json!({
            "suite_id": suite_id,
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
                "benchmark_id": selected_benchmark_id,
            },
            "remaining": [
                {
                    "run_index": 1,
                    "profile": "managed_default",
                    "run_type": "repeat",
                    "target_id": null,
                    "benchmark_id": remaining_benchmark_id,
                }
            ],
        })
    );
}

#[test]
fn benchmark_suite_release_validation_carries_family_c_target_identity() {
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let suite_id = test_suite_id("family-c-target-identity", "release_validation");
    let payload = benchmark_suite_status_payload(&suite_id, "release_validation", 0, &plan);
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

#[tokio::test]
async fn benchmark_suite_manifest_persists_family_c_target_identity() {
    let store = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
    let suite_id =
        crate::state::benchmark_suites::derive_suite_id("instance", "release_validation");
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);

    persist_suite_run(
        &store,
        &suite_id,
        "instance",
        "release_validation",
        &manifest_runs,
        1,
        "session-1",
        "2026-01-01T00:00:00.000Z",
    )
    .await
    .expect("persist launched run");
    let manifest = store
        .get(&suite_id)
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
}

#[tokio::test]
async fn every_benchmark_suite_plan_passes_strict_store_admission() {
    let store = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
    for mode in ["development", "qualification", "release_validation"] {
        let suite_id = crate::state::benchmark_suites::derive_suite_id("instance", mode);
        let plan = benchmark_suite_plan(mode).expect("suite plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs(mode, &plan);

        assert!(manifest_runs.iter().all(|run| run.benchmark_id.len() < 48));
        let manifest = persist_suite_run(
            &store,
            &suite_id,
            "instance",
            mode,
            &manifest_runs,
            0,
            &format!("session-{mode}"),
            "2026-01-01T00:00:00.000Z",
        )
        .await
        .expect("strict store accepts current suite plan");

        assert_eq!(manifest.suite_id, suite_id);
        assert_eq!(manifest.runs.len(), plan.len());
    }
}

#[test]
fn benchmark_suite_ids_are_opaque_and_change_with_family_c_target_identity() {
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let baseline_id = benchmark_suite_run_id("release_validation", 0, plan[0]);
    let managed_id = benchmark_suite_run_id("release_validation", 1, plan[1]);

    assert_ne!(baseline_id, managed_id);
    for benchmark_id in [baseline_id, managed_id] {
        assert!(benchmark_id.starts_with("benchmark-"));
        assert!(benchmark_id.len() < 48);
        assert!(!benchmark_id.contains("release_validation"));
        assert!(!benchmark_id.contains("family_c"));
        assert!(!benchmark_id.contains("vanilla_baseline"));
        assert!(!benchmark_id.contains("managed_default"));
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
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let expected_baseline_benchmark_id = benchmark_suite_run_id("release_validation", 0, plan[0]);
    let expected_managed_benchmark_id = benchmark_suite_run_id("release_validation", 1, plan[1]);
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(crate::state::launch_reports::LaunchProofComparison {
            baseline_session_id: "baseline-session".to_string(),
            baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            baseline: family_c_comparison_baseline(),
            matched_sample_count: 1,
            metric_name: "total_completed_stage_duration_ms".to_string(),
            current_value_ms: 90,
            baseline_value_ms: 120,
            delta_ms: -30,
            delta_percent: -25.0,
        }),
    );
    write_family_c_managed_state(&fixture, &instance_id);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
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
    assert_eq!(
        payload["targets"][0]["proof"]["benchmark_id"],
        serde_json::json!(expected_baseline_benchmark_id)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["benchmark_id"],
        serde_json::json!(expected_managed_benchmark_id)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["baseline_matches"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["metric_valid"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["samples_present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["values_present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][0]["proof"]["resource_budget"],
        serde_json::json!({
            "present": true,
            "memory": true,
            "cpu": true,
            "install": true,
            "disk": true,
        })
    );
    assert_eq!(
        payload["targets"][0]["proof"]["guardian"],
        serde_json::json!({
            "present": true,
            "decision": "allowed",
        })
    );
    assert_eq!(
        payload["targets"][1]["managed_install"],
        serde_json::json!({
            "required": true,
            "present": true,
            "composition_id": "family-c-forge-core",
            "installed_count": 3,
            "expected_artifacts_present": true,
            "ownership": true,
            "source": true,
            "integrity": true,
        })
    );
    assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
    assert_eq!(payload["targets"][1]["missing"], serde_json::json!([]));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_rejects_wrong_opaque_benchmark_id() {
    let fixture = RouteTestFixture::new("family-c-qualification-wrong-benchmark-id");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let mut manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);
    let wrong_plan = benchmark_suite_plan("development").expect("development plan");
    let wrong_benchmark_id = benchmark_suite_run_id("development", 0, wrong_plan[0]);
    assert_ne!(wrong_benchmark_id, manifest_runs[0].benchmark_id);
    manifest_runs[0].benchmark_id = wrong_benchmark_id.clone();
    persist_suite_run(
        fixture.state.benchmark_suites(),
        &suite_id,
        &instance_id,
        "release_validation",
        &manifest_runs,
        0,
        "baseline-session",
        "2026-01-01T00:00:00.000Z",
    )
    .await
    .expect("persist suite with wrong opaque benchmark id");
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
        .expect("load suite")
        .expect("suite exists");
    let baseline_run = manifest
        .runs
        .iter()
        .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
        .expect("baseline run");
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(
        payload["targets"][0]["missing"],
        serde_json::json!(["suite_run_benchmark_id_mismatch"])
    );
    assert_eq!(
        payload["targets"][0]["proof"]["benchmark_id"],
        serde_json::json!(wrong_benchmark_id)
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_managed_comparison_must_match_suite_baseline() {
    let fixture = RouteTestFixture::new("family-c-qualification-wrong-comparison-baseline");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    let mut comparison = family_c_comparison();
    comparison.baseline_session_id = "unrelated-baseline-session".to_string();
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(comparison),
    );
    write_family_c_managed_state(&fixture, &instance_id);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
    assert_eq!(
        payload["targets"][1]["missing"],
        serde_json::json!(["managed_comparison_baseline_mismatch"])
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["baseline_matches"],
        serde_json::json!(false)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["metric_valid"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["samples_present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["comparison"]["values_present"],
        serde_json::json!(true)
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_missing_managed_state_only_blocks_managed_target() {
    let fixture = RouteTestFixture::new("family-c-qualification-missing-managed-state");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(family_c_comparison()),
    );

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
    assert_eq!(
        payload["targets"][1]["missing"],
        serde_json::json!(["managed_install_state_missing"])
    );
    assert_eq!(
        payload["targets"][1]["managed_install"]["present"],
        serde_json::json!(false)
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_invalid_managed_state_only_blocks_managed_target() {
    let fixture = RouteTestFixture::new("family-c-qualification-invalid-managed-state");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(family_c_comparison()),
    );
    write_invalid_family_c_managed_state(&fixture, &instance_id);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
    assert_eq!(
        payload["targets"][1]["missing"],
        serde_json::json!(["managed_install_state_invalid"])
    );
    assert_eq!(
        payload["targets"][1]["managed_install"]["present"],
        serde_json::json!(false)
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_proofs_without_guardian_and_resource_budget_are_incomplete() {
    let fixture = RouteTestFixture::new("family-c-qualification-missing-proof-evidence");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    let mut baseline_proof = family_c_proof_record(baseline_run, &instance_id, "vanilla", None);
    baseline_proof.resource_budget = None;
    baseline_proof.guardian = None;
    write_family_c_proof_record(&fixture, &baseline_proof);
    let mut managed_proof = family_c_proof_record(
        managed_run,
        &instance_id,
        "managed",
        Some(crate::state::launch_reports::LaunchProofComparison {
            baseline_session_id: "baseline-session".to_string(),
            baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            baseline: family_c_comparison_baseline(),
            matched_sample_count: 1,
            metric_name: "total_completed_stage_duration_ms".to_string(),
            current_value_ms: 90,
            baseline_value_ms: 120,
            delta_ms: -30,
            delta_percent: -25.0,
        }),
    );
    managed_proof.resource_budget = None;
    managed_proof.guardian = Some(serde_json::json!({
        "mode": "managed",
        "decision": "   ",
    }));
    write_family_c_proof_record(&fixture, &managed_proof);
    write_family_c_managed_state(&fixture, &instance_id);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    let expected_baseline_missing = serde_json::json!([
        "proof_guardian_missing",
        "proof_resource_budget_missing",
        "proof_resource_cpu_evidence_missing",
        "proof_resource_disk_evidence_missing",
        "proof_resource_install_evidence_missing",
        "proof_resource_memory_evidence_missing"
    ]);
    let expected_managed_missing = serde_json::json!([
        "proof_guardian_decision_missing",
        "proof_resource_budget_missing",
        "proof_resource_cpu_evidence_missing",
        "proof_resource_disk_evidence_missing",
        "proof_resource_install_evidence_missing",
        "proof_resource_memory_evidence_missing"
    ]);
    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(payload["targets"][0]["missing"], expected_baseline_missing);
    assert_eq!(payload["targets"][1]["missing"], expected_managed_missing);
    assert_eq!(
        payload["targets"][0]["proof"]["resource_budget"],
        serde_json::json!({
            "present": false,
            "memory": false,
            "cpu": false,
            "install": false,
            "disk": false,
        })
    );
    assert_eq!(
        payload["targets"][0]["proof"]["guardian"],
        serde_json::json!({ "present": false })
    );
    assert_eq!(
        payload["targets"][1]["proof"]["guardian"],
        serde_json::json!({
            "present": true,
            "decision": null,
        })
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_missing_baseline_and_managed_evidence_is_incomplete() {
    let fixture = RouteTestFixture::new("family-c-qualification-incomplete");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(&fixture.state, &suite_id, &instance_id, 2, "legacy-session").await;

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
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
async fn family_c_qualification_does_not_reuse_stale_same_benchmark_proofs() {
    let fixture = RouteTestFixture::new("family-c-qualification-stale-proof");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "current-baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "current-managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
        .expect("load suite")
        .expect("suite exists");
    let mut stale_baseline_run = manifest
        .runs
        .iter()
        .find(|run| run.target_id == FAMILY_C_BASELINE_TARGET_ID)
        .expect("baseline run")
        .clone();
    stale_baseline_run.session_id = Some("stale-baseline-session".to_string());
    let stale_baseline = family_c_proof_record(&stale_baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof_record(&fixture, &stale_baseline);
    let mut stale_managed_run = manifest
        .runs
        .iter()
        .find(|run| run.target_id == FAMILY_C_MANAGED_TARGET_ID)
        .expect("managed run")
        .clone();
    stale_managed_run.session_id = Some("stale-managed-session".to_string());
    let mut stale_comparison = family_c_comparison();
    stale_comparison.baseline_session_id = "stale-baseline-session".to_string();
    let stale_managed = family_c_proof_record(
        &stale_managed_run,
        &instance_id,
        "managed",
        Some(stale_comparison),
    );
    write_family_c_proof_record(&fixture, &stale_managed);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert!(
        payload["targets"][0]["missing"]
            .as_array()
            .expect("baseline missing list")
            .contains(&serde_json::json!("proof_missing"))
    );
    assert!(
        payload["targets"][1]["missing"]
            .as_array()
            .expect("managed missing list")
            .contains(&serde_json::json!("proof_missing"))
    );
    assert_eq!(
        payload["targets"][0]["proof"]["present"],
        serde_json::json!(false)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["present"],
        serde_json::json!(false)
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
async fn launch_preflight_route_returns_ready_without_creating_session() {
    let fixture = RouteTestFixture::new("launch-preflight-ready-route");
    fixture.configure_library();
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
        instance.extra_jvm_args = "-Dtoken=secret-token -XX:+UseZGC".to_string();
    });

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/launch/preflight/{instance_id}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("preflight json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert_eq!(payload["status"], serde_json::json!("ready"));
    assert_eq!(
        payload["overrides"]["java"]["present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["overrides"]["java"]["origin"],
        serde_json::json!("instance")
    );
    assert_eq!(
        payload["overrides"]["raw_jvm_args"]["present"],
        serde_json::json!(true)
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    assert!(!data.contains("/Users/SecretUser"));
    assert!(!data.contains("manual/bin/java"));
    assert!(!data.contains("-Dtoken"));
    assert!(!data.contains("secret-token"));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_preflight_route_missing_instance_returns_json_404() {
    let fixture = RouteTestFixture::new("launch-preflight-missing-route");
    fixture.configure_library();

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/launch/preflight/missing-instance")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("error json");
    assert_eq!(
        payload,
        serde_json::json!({ "error": "instance not found" })
    );

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_preflight_route_serializes_guardian_java_and_jvm_override_outcome() {
    let fixture = RouteTestFixture::new("launch-preflight-java-jvm-guardian-route");
    fixture.configure_library();
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Guardian Overrides", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "null".to_string();
        instance.extra_jvm_args = "-cp /Users/Alice/secret.jar -Dtoken=secret-token".to_string();
    });

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/launch/preflight/{instance_id}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("preflight json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert_eq!(payload["status"], serde_json::json!("ready"));
    assert_eq!(payload["readiness"]["launchable"], serde_json::json!(true));
    assert_eq!(
        payload["guardian"]["decision"],
        serde_json::json!("intervened")
    );
    assert_eq!(
        payload["overrides"]["java"],
        serde_json::json!({
            "present": true,
            "origin": "instance",
        })
    );
    assert_eq!(
        payload["overrides"]["raw_jvm_args"],
        serde_json::json!({
            "present": true,
            "origin": "instance",
        })
    );
    let facts = payload["guardian_facts"]
        .as_array()
        .expect("guardian facts array");
    assert!(facts.iter().any(|fact| {
        fact["id"] == "java_override_undefined_sentinel"
            && fact["domain"] == "Runtime"
            && fact["target"]["id"] == "instance_java_override"
            && fact["fields"].as_array().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field["key"] == "sentinel" && field["value"] == "null")
            })
    }));
    assert!(facts.iter().any(|fact| {
        fact["id"] == "jvm_arg_unsafe_classpath_override"
            && fact["domain"] == "Jvm"
            && fact["target"]["id"] == "explicit_jvm_args"
    }));
    assert!(
        payload["guardian"]["guidance"]
            .as_array()
            .is_some_and(|guidance| guidance.iter().any(|detail| detail.as_str() == Some(
                "Guardian will ignore the unavailable Java override and use managed Java for this launch."
            )))
    );
    assert!(
        payload["guardian"]["guidance"]
            .as_array()
            .is_some_and(|guidance| guidance.iter().any(|detail| detail.as_str() == Some(
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly."
            )))
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(!data.contains("/Users/Alice"));
    assert!(!data.contains("secret.jar"));
    assert!(!data.contains("-cp"));
    assert!(!data.contains("-Dtoken"));
    assert!(!data.contains("secret-token"));
    assert!(!data.contains("java_path"));
    assert!(!data.contains("command"));
    assert!(!data.contains("requested_java"));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_route_online_auth_unready_returns_backend_notice_without_session() {
    let fixture = RouteTestFixture::new("launch-online-auth-unready-route");
    fixture.configure_library();
    fixture.set_launch_auth_mode("online");
    let instance_id = fixture.add_instance("Online Auth", "1.21.1");
    let request_lease = fixture.state.try_admit_request().expect("admit request");

    let response = router()
        .layer(Extension(request_lease.producer_handoff()))
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/launch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "instance_id": instance_id,
                        "username": null,
                        "max_memory_mb": null,
                        "min_memory_mb": null,
                        "client_started_at_ms": null
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&body).expect("launch error json");
    let data = String::from_utf8(body.to_vec()).expect("utf8 payload");

    assert_eq!(
        payload["error"],
        "Online launch requires an active verified Minecraft Java account"
    );
    assert_eq!(payload["failure_class"], "auth_mode_incompatible");
    assert_eq!(payload["launch_auth_mode"], "online");
    assert_eq!(payload["online_mode_ready"], false);
    assert_eq!(payload["auth_refresh_status"], "sign_in_required");
    assert_eq!(payload["auth_refresh_reason"], "refresh_token_missing");
    assert_eq!(
        payload["notice"]["message"],
        "Online launch needs you to sign in again."
    );
    assert_eq!(payload["notice"]["tone"], "error");
    assert!(
        payload["notice"]["details"]
            .as_array()
            .is_some_and(|details| {
                details.iter().any(|detail| {
                    detail
                        .as_str()
                        .is_some_and(|value| value.contains("Sign in again from Accounts"))
                })
            })
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(!data.contains("access_token"));
    assert!(!data.contains("\"refresh_token\""));
    assert!(!data.contains("id_token"));
    assert!(!data.contains("secret"));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn launch_route_returns_bounded_503_without_spawning_after_shutdown_rejection() {
    let fixture = RouteTestFixture::new("launch-shutdown-admission-route");
    fixture.configure_library();
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Shutdown admission", "1.21.1");
    fixture
        .state
        .sessions()
        .terminate_all()
        .await
        .expect("latch session shutdown");
    let request_lease = fixture.state.try_admit_request().expect("admit request");

    let response = router()
        .layer(Extension(request_lease.producer_handoff()))
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/launch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "instance_id": instance_id,
                        "username": null,
                        "max_memory_mb": null,
                        "min_memory_mb": null,
                        "client_started_at_ms": null
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("launch error json"),
        serde_json::json!({
            "error": "Launches are unavailable while the application is shutting down."
        })
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
}

#[tokio::test]
async fn family_c_qualification_route_returns_ready_for_complete_suite() {
    let fixture = RouteTestFixture::new("family-c-qualification-ready-route");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(crate::state::launch_reports::LaunchProofComparison {
            baseline_session_id: "baseline-session".to_string(),
            baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            baseline: family_c_comparison_baseline(),
            matched_sample_count: 1,
            metric_name: "total_completed_stage_duration_ms".to_string(),
            current_value_ms: 90,
            baseline_value_ms: 120,
            delta_ms: -30,
            delta_percent: -25.0,
        }),
    );
    write_family_c_managed_state(&fixture, &instance_id);

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
    assert_eq!(
        payload["targets"][1]["proof"]["resource_budget"]["present"],
        serde_json::json!(true)
    );
    assert_eq!(
        payload["targets"][1]["proof"]["guardian"],
        serde_json::json!({
            "present": true,
            "decision": "allowed",
        })
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
    let payload = family_c_qualification_preview_payload().expect("family c qualification preview");
    let data = serde_json::to_string(&payload).expect("serialize payload");
    let lower_data = data.to_ascii_lowercase();
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let baseline_benchmark_id = benchmark_suite_run_id("release_validation", 0, plan[0]);
    let managed_benchmark_id = benchmark_suite_run_id("release_validation", 1, plan[1]);

    assert_eq!(payload["status"], serde_json::json!("incomplete"));
    assert_eq!(
        payload["view_model"],
        serde_json::json!({
            "status_label": "Incomplete",
            "status_tone": "warn",
            "target_label": "Family C, Forge, 1.12.2, Release Validation",
            "suite_label": "Suite missing",
            "schema_label": "v1",
            "missing_summary": "7 missing: Proof Missing, Suite Manifest Missing, +5",
            "suite_summary": "Suite missing",
            "evidence_summary": "Baseline: Pending, run #1, Proof missing | Managed: Pending, run #2, Proof missing",
        })
    );
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
        payload["targets"][0]["view_model"],
        serde_json::json!({
            "role_label": "Baseline",
            "target_label": "Family C, Forge, 1.12.2, Release Validation",
            "required_label": "Vanilla Baseline | Coldish | Release Validation | Vanilla",
            "suite_label": "Pending, run #1",
            "suite_present": true,
            "proof_label": "Proof missing",
            "proof_present": false,
            "missing_label": "3 missing",
            "missing_tone": "warn",
        })
    );
    assert_eq!(
        payload["targets"][0]["suite_run"]["benchmark_id"],
        serde_json::json!(baseline_benchmark_id)
    );
    assert_eq!(
        payload["targets"][1]["suite_run"]["benchmark_id"],
        serde_json::json!(managed_benchmark_id)
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
async fn family_c_qualification_uses_committed_memory_not_external_manifest_rewrite() {
    let fixture = RouteTestFixture::new("family-c-qualification-wrong-mode");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let mut manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    write_family_c_proof(
        &fixture,
        managed_run,
        &instance_id,
        "managed",
        Some(crate::state::launch_reports::LaunchProofComparison {
            baseline_session_id: "baseline-session".to_string(),
            baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            baseline: family_c_comparison_baseline(),
            matched_sample_count: 1,
            metric_name: "total_completed_stage_duration_ms".to_string(),
            current_value_ms: 90,
            baseline_value_ms: 120,
            delta_ms: -30,
            delta_percent: -25.0,
        }),
    );
    write_family_c_managed_state(&fixture, &instance_id);
    manifest.mode = "development".to_string();
    write_family_c_suite_manifest(&fixture.paths, &manifest);

    let manifest_payload = benchmark_suite_manifest(&fixture.state, &suite_id)
        .expect("manifest payload uses committed memory");
    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
        .await
        .expect("qualification payload");

    assert_eq!(
        manifest_payload["mode"],
        serde_json::json!("release_validation")
    );
    assert_eq!(payload["status"], serde_json::json!("ready"));
    assert_eq!(
        payload["suite"]["mode"],
        serde_json::json!("release_validation")
    );
    assert_eq!(payload["targets"][0]["missing"], serde_json::json!([]));
    assert_eq!(payload["targets"][1]["missing"], serde_json::json!([]));

    cleanup(&fixture.root);
}

#[tokio::test]
async fn family_c_qualification_payload_excludes_sensitive_fields() {
    let fixture = RouteTestFixture::new("family-c-qualification-sensitive");
    let instance_id = fixture.add_instance("Family C", FAMILY_C_QUALIFICATION_VERSION);
    let suite_id = test_suite_id(&instance_id, "release_validation");
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        0,
        "baseline-session",
    )
    .await;
    persist_family_c_suite_run(
        &fixture.state,
        &suite_id,
        &instance_id,
        1,
        "managed-session",
    )
    .await;
    let manifest = fixture
        .state
        .benchmark_suites()
        .get(&suite_id)
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
    write_family_c_proof(&fixture, baseline_run, &instance_id, "vanilla", None);
    let mut managed_proof = family_c_proof_record(
        managed_run,
        &instance_id,
        "managed",
        Some(crate::state::launch_reports::LaunchProofComparison {
            baseline_session_id: "baseline-session".to_string(),
            baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            baseline: family_c_comparison_baseline(),
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
    managed_proof.guardian = Some(serde_json::json!({
        "mode": "managed",
        "decision": "allowed",
        "details": ["C:\\Users\\SecretUser\\token --runtime-arguments"],
    }));
    write_family_c_proof_record(&fixture, &managed_proof);
    write_family_c_managed_state(&fixture, &instance_id);

    let payload = family_c_qualification_payload(&fixture.state, &suite_id)
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

    let expected_explicit = crate::state::benchmark_suites::normalize_suite_id("../chosen suite")
        .expect("normalized explicit suite id");
    assert_eq!(explicit.suite_id, expected_explicit);
    assert_eq!(
        derived.suite_id,
        crate::state::benchmark_suites::derive_suite_id("instance", "development")
    );
    for suite_id in [&explicit.suite_id, &derived.suite_id] {
        assert!(suite_id.chars().count() < 48);
        assert_eq!(
            crate::state::benchmark_suites::normalize_suite_id(suite_id).as_deref(),
            Some(suite_id.as_str())
        );
        assert!(!suite_id.contains("chosen"));
        assert!(!suite_id.contains("instance"));
        assert!(!suite_id.contains('/'));
        assert!(!suite_id.contains(' '));
    }
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
        payload["view_model"],
        serde_json::json!({
            "state_label": "Scheduled",
            "state_tone": "info",
            "can_stop": true,
            "can_resume": false,
            "can_check_family_c_qualification": true,
        })
    );
    assert_eq!(
        payload["driver"]["mode"],
        serde_json::json!("release_validation")
    );
    assert_eq!(
        payload["suite"]["mode"],
        serde_json::json!("release_validation")
    );
    let suite_id = payload["driver"]["suite_id"].as_str().expect("suite id");
    let expected_suite_id = crate::state::benchmark_suites::normalize_suite_id("../driver suite")
        .expect("normalized driver suite id");
    assert_eq!(suite_id, expected_suite_id);
    assert!(suite_id.chars().count() < 48);
    assert_eq!(
        crate::state::benchmark_suites::normalize_suite_id(suite_id).as_deref(),
        Some(suite_id)
    );
    assert!(!suite_id.contains("driver"));
    assert!(!suite_id.contains('/'));
    assert!(!suite_id.contains(' '));
}

#[tokio::test]
async fn benchmark_suite_driver_duplicate_start_conflicts_until_terminal() {
    let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
    let suite_id = test_suite_id("driver-duplicate", "development");
    let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
        run_count: 2,
        launched_run_count: 0,
        pending_run_index: Some(0),
    };
    let first = store
        .start(
            suite_id.clone(),
            "development".to_string(),
            30_000,
            summary.clone(),
        )
        .await
        .expect("first driver starts");

    let conflict = store
        .start(
            suite_id.clone(),
            "development".to_string(),
            30_000,
            summary.clone(),
        )
        .await;

    assert!(conflict.is_err());
    store
        .record_stopped(&first.status.id)
        .await
        .expect("first driver stops");
    drop(first.effect_owner);
    store
        .start(suite_id, "development".to_string(), 30_000, summary)
        .await
        .expect("terminal driver no longer conflicts");
}

#[tokio::test]
async fn benchmark_suite_driver_stop_reports_stopped_without_killing_sessions() {
    let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
    let suite_id = test_suite_id("driver-stop", "development");
    let sessions = crate::state::SessionStore::new();
    sessions
        .insert(test_record("active-suite-session"))
        .await
        .expect("insert session");
    let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
        run_count: 2,
        launched_run_count: 1,
        pending_run_index: Some(1),
    };
    let started = store
        .start(suite_id, "development".to_string(), 30_000, summary)
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
                test_suite_id(&format!("driver-list-{index}"), "development"),
                "development".to_string(),
                30_000,
                summary.clone(),
            )
            .await
            .expect("driver starts");
        store
            .record_stopped(&started.status.id)
            .await
            .expect("driver stops");
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
    assert_eq!(
        payload["drivers"][0]["view_model"],
        serde_json::json!({
            "state_label": "Stopped",
            "state_tone": "warn",
            "can_stop": false,
            "can_resume": true,
            "can_check_family_c_qualification": false,
        })
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
        fixture.state.try_claim_producer().expect("claim producer"),
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
    let suite_id = test_suite_id("driver-resume-active", "development");
    let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
        run_count: 2,
        launched_run_count: 0,
        pending_run_index: Some(0),
    };
    let started = fixture
        .state
        .benchmark_suite_drivers()
        .start(suite_id, "development".to_string(), 30_000, summary)
        .await
        .expect("driver starts");

    let error = resume_benchmark_suite_driver(
        fixture.state.clone(),
        started.status.id,
        fixture.state.try_claim_producer().expect("claim producer"),
    )
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
    let suite_id = test_suite_id("driver-resume-missing-manifest", "development");
    let stopped = fixture
        .stopped_driver(&suite_id, "development", 30_000)
        .await;

    let error = resume_benchmark_suite_driver(
        fixture.state.clone(),
        stopped.id,
        fixture.state.try_claim_producer().expect("claim producer"),
    )
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
    let suite_id = test_suite_id("driver-resume-complete-manifest", "development");
    fixture.persist_suite_runs(&suite_id, &[0, 1]).await;
    let stopped = fixture
        .stopped_driver(&suite_id, "development", 30_000)
        .await;

    let error = resume_benchmark_suite_driver(
        fixture.state.clone(),
        stopped.id,
        fixture.state.try_claim_producer().expect("claim producer"),
    )
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
    let suite_id = test_suite_id("driver-resume-success", "development");
    fixture.persist_suite_runs(&suite_id, &[0]).await;
    fixture
        .state
        .sessions()
        .insert(test_record("session-0"))
        .await
        .expect("insert session");
    let stopped = fixture
        .stopped_driver(&suite_id, "development", 45_000)
        .await;

    let payload = resume_benchmark_suite_driver(
        fixture.state.clone(),
        stopped.id.clone(),
        fixture.state.try_claim_producer().expect("claim producer"),
    )
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
    let suite_id = test_suite_id("driver-auto-resume-success", "development");
    fixture.persist_suite_runs(&suite_id, &[0]).await;
    let interrupted = fixture
        .active_driver(&suite_id, "development", 45_000)
        .await;
    let reloaded = fixture.reload().await;
    reloaded
        .state
        .sessions()
        .insert(test_record("session-0"))
        .await
        .expect("insert session");

    let summary = resume_restart_interrupted_benchmark_suite_drivers(
        reloaded.state.clone(),
        reloaded.state.try_claim_producer().expect("claim producer"),
    )
    .await
    .expect("restart driver reconciliation");

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
    let suite_id = test_suite_id("driver-auto-resume-missing-manifest", "development");
    let interrupted = fixture
        .active_driver(&suite_id, "development", 30_000)
        .await;
    let reloaded = fixture.reload().await;

    let summary = resume_restart_interrupted_benchmark_suite_drivers(
        reloaded.state.clone(),
        reloaded.state.try_claim_producer().expect("claim producer"),
    )
    .await
    .expect("restart driver reconciliation");

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
    let suite_id = test_suite_id("driver-auto-resume-complete-manifest", "development");
    fixture.persist_suite_runs(&suite_id, &[0, 1]).await;
    let interrupted = fixture
        .active_driver(&suite_id, "development", 30_000)
        .await;
    let reloaded = fixture.reload().await;

    let summary = resume_restart_interrupted_benchmark_suite_drivers(
        reloaded.state.clone(),
        reloaded.state.try_claim_producer().expect("claim producer"),
    )
    .await
    .expect("restart driver reconciliation");

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
    let suite_id = test_suite_id("driver-error", "development");
    let summary = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
        run_count: 2,
        launched_run_count: 0,
        pending_run_index: Some(0),
    };
    let started = store
        .start(suite_id, "development".to_string(), 30_000, summary)
        .await
        .expect("driver starts");
    store
        .record_failed(
            &started.status.id,
            "failed command java_path /home/Secret/.minecraft --jvm-args username Secret",
        )
        .await
        .expect("failed driver status persists");
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

#[tokio::test]
async fn benchmark_suite_driver_response_defensively_redacts_tampered_fields() {
    let store = crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverStore::new();
    let suite_id = test_suite_id("driver-tampered", "development");
    let started = store
        .start(
            suite_id,
            "development".to_string(),
            30_000,
            crate::state::benchmark_suite_drivers::BenchmarkSuiteDriverSuiteSummary {
                run_count: 2,
                launched_run_count: 0,
                pending_run_index: Some(0),
            },
        )
        .await
        .expect("driver starts");
    let mut status = started.status.clone();
    drop(started.effect_owner);
    status.id = "/home/Secret/driver".to_string();
    status.state = "../../raw-state".to_string();
    status.suite_id = "C:\\Users\\Secret\\suite".to_string();
    status.mode = "secret-mode".to_string();
    status.active_session_id = Some("/home/Secret/access-token".to_string());
    status.last_session_id = Some("C:\\Users\\Secret\\session".to_string());
    status.error = Some("failed /home/Secret --access-token raw-secret".to_string());
    status.created_at = "raw-created-/home/Secret".to_string();
    status.updated_at = "raw-updated-C:\\Users\\Secret".to_string();
    status.run_count = usize::MAX;
    status.launched_run_count = usize::MAX;
    status.pending_run_index = Some(usize::MAX);

    let payload = benchmark_suite_driver_response_payload("../../raw-status", &status);
    let serialized = serde_json::to_string(&payload).expect("serialize public response");

    assert_eq!(payload["status"], serde_json::json!("unknown"));
    assert_eq!(payload["driver"]["state"], serde_json::json!("unknown"));
    assert_eq!(payload["driver"]["suite_id"], serde_json::json!("suite"));
    assert_eq!(payload["driver"]["mode"], serde_json::json!("unknown"));
    assert_eq!(
        payload["view_model"]["state_label"],
        serde_json::json!("Unknown")
    );
    assert_eq!(
        payload["driver"]["created_at"],
        serde_json::json!("unknown")
    );
    assert_eq!(
        payload["driver"]["updated_at"],
        serde_json::json!("unknown")
    );
    assert_eq!(
        payload["driver"]["error"],
        serde_json::json!("driver error")
    );
    assert_eq!(payload["suite"]["run_count"], serde_json::json!(64));
    assert_eq!(
        payload["suite"]["pending_run_index"],
        serde_json::Value::Null
    );
    assert!(!serialized.contains("Secret"));
    assert!(!serialized.contains("raw-state"));
    assert!(!serialized.contains("raw-status"));
    assert!(!serialized.contains("access-token"));
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
async fn benchmark_suite_tick_active_returns_non_error_without_launching() {
    let store = crate::state::SessionStore::new();
    store
        .insert(test_record("active-suite-session"))
        .await
        .expect("insert session");
    let plan = benchmark_suite_plan("development").expect("development plan");
    let suite_id = test_suite_id("tick-active", "development");
    let input = BenchmarkSuitePlanInput {
        launch: launch_app::LaunchRequest {
            instance_id: "instance".to_string(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
        suite_id: suite_id.clone(),
        mode: "development".to_string(),
        plan,
        manifest: Some(test_manifest(
            &suite_id,
            vec![test_manifest_run(0, Some("active-suite-session"))],
        )),
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
            assert_eq!(suite["suite_id"], suite_id);
            assert_eq!(suite["pending_run_index"], serde_json::json!(1));
        }
        BenchmarkSuiteDriverDecision::Complete { .. } | BenchmarkSuiteDriverDecision::Launch(_) => {
            panic!("active manifest run should not launch or complete")
        }
    }
}

#[tokio::test]
async fn benchmark_suite_tick_complete_returns_non_error_without_launching() {
    let store = crate::state::SessionStore::new();
    let plan = benchmark_suite_plan("development").expect("development plan");
    let suite_id = test_suite_id("tick-complete", "development");
    let input = BenchmarkSuitePlanInput {
        launch: launch_app::LaunchRequest {
            instance_id: "instance".to_string(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
        suite_id: suite_id.clone(),
        mode: "development".to_string(),
        plan,
        manifest: Some(test_manifest(
            &suite_id,
            vec![
                test_manifest_run(0, Some("session-0")),
                test_manifest_run(1, Some("session-1")),
            ],
        )),
    };

    let decision = benchmark_suite_driver_decision(&store, input)
        .await
        .expect("tick decision should succeed");

    match decision {
        BenchmarkSuiteDriverDecision::Complete { suite } => {
            assert_eq!(suite["suite_id"], suite_id);
            assert_eq!(suite["pending_run_index"], serde_json::Value::Null);
            assert_eq!(suite["launched_run_count"], serde_json::json!(2));
        }
        BenchmarkSuiteDriverDecision::Active { .. } | BenchmarkSuiteDriverDecision::Launch(_) => {
            panic!("complete manifest should not launch or report active")
        }
    }
}

#[tokio::test]
async fn benchmark_suite_tick_selects_next_unlaunched_manifest_run() {
    let store = crate::state::SessionStore::new();
    let suite_store = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
    let suite_id = test_suite_id("tick-pending", "development");
    let plan = benchmark_suite_plan("development").expect("development plan");
    let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
    persist_suite_run(
        &suite_store,
        &suite_id,
        "instance",
        "development",
        &manifest_runs,
        0,
        "session-0",
        "2026-01-01T00:00:00.000Z",
    )
    .await
    .expect("persist launched run");
    let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
        "instance_id": "instance",
        "suite_mode": "development",
        "suite_id": &suite_id
    }))
    .expect("deserialize suite tick request");
    let input = request
        .into_suite_plan_input_with_manifest(Some(&suite_store))
        .expect("suite input should parse");

    let decision = benchmark_suite_driver_decision(&store, input)
        .await
        .expect("tick decision should succeed");

    match decision {
        BenchmarkSuiteDriverDecision::Launch(input) => {
            assert_eq!(input.requested_run_index, None);
            assert_eq!(input.suite_id, suite_id);
        }
        BenchmarkSuiteDriverDecision::Active { .. }
        | BenchmarkSuiteDriverDecision::Complete { .. } => {
            panic!("pending manifest should launch next run")
        }
    }
}

#[tokio::test]
async fn benchmark_suite_tick_without_manifest_starts_first_run() {
    let store = crate::state::SessionStore::new();
    let suite_store = crate::state::benchmark_suites::BenchmarkSuiteStore::new();
    let suite_id = test_suite_id("tick-no-manifest", "development");
    let request: BenchmarkLaunchRequest = serde_json::from_value(serde_json::json!({
        "instance_id": "instance",
        "suite_mode": "development",
        "suite_id": &suite_id
    }))
    .expect("deserialize suite tick request");
    let input = request
        .into_suite_plan_input_with_manifest(Some(&suite_store))
        .expect("suite input should parse");

    let decision = benchmark_suite_driver_decision(&store, input)
        .await
        .expect("tick decision should succeed");

    match decision {
        BenchmarkSuiteDriverDecision::Launch(input) => {
            assert_eq!(input.requested_run_index, None);
            assert_eq!(input.plan.len(), 2);
        }
        BenchmarkSuiteDriverDecision::Active { .. }
        | BenchmarkSuiteDriverDecision::Complete { .. } => {
            panic!("missing manifest should launch first run")
        }
    }
}

#[test]
fn benchmark_suite_tick_status_payload_excludes_sensitive_fields() {
    let plan = benchmark_suite_plan("development").expect("development plan");
    let suite_id = test_suite_id("tick-status-sensitive", "development");
    let manifest = test_manifest(&suite_id, vec![test_manifest_run(0, Some("session-0"))]);
    let payload = serde_json::json!({
        "status": "active",
        "driver": { "state": "active" },
        "suite": benchmark_suite_driver_status_payload(
            &suite_id,
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
    let run_index = input.requested_run_index.expect("explicit run index");
    let selected = input.plan[run_index];
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some(benchmark_suite_run_id(&input.mode, run_index, selected).as_str()),
        Some(selected.profile),
        Some(selected.run_type),
        Some(input.mode.as_str()),
    );
    let payload = serde_json::json!({
        "benchmark": launch_app::launch_benchmark_status_payload(&benchmark),
        "suite": benchmark_suite_status_payload(
            &input.suite_id,
            &input.mode,
            run_index,
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
        launch_app::launch_benchmark_status_payload(&benchmark),
        serde_json::json!({
            "id": "benchmark-1",
            "profile": "dev-default",
            "run_type": "repeat",
            "mode": "release_validation",
        })
    );
}

#[test]
fn benchmark_status_payload_drops_sensitive_client_metadata() {
    let benchmark = crate::state::launch_reports::LaunchBenchmarkMetadata::new(
        Some("/home/alice/token=secret"),
        Some("C:\\Users\\Alice\\profile"),
        Some("--access-token raw-secret"),
        Some("release_validation"),
    );
    let payload = launch_app::launch_benchmark_status_payload(&benchmark);
    let serialized = serde_json::to_string(&payload).expect("serialize benchmark status");

    assert_eq!(payload["id"], serde_json::Value::Null);
    assert_eq!(payload["profile"], serde_json::Value::Null);
    assert_eq!(payload["run_type"], serde_json::Value::Null);
    assert!(!serialized.contains("alice"));
    assert!(!serialized.contains("secret"));
}

fn sensitive_launch_proof_record() -> crate::state::launch_reports::LaunchProofRecord {
    crate::state::launch_reports::LaunchProofRecord {
            schema: "axial.launch.proof".to_string(),
            schema_version: 2,
            session_id: "sensitive-proof".to_string(),
            instance_id: "instance-safe".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: "2026-01-01T00:00:00.000Z".to_string(),
            recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
            outcome: "failed".to_string(),
            session_outcome: None,
            scenario: crate::state::launch_reports::LaunchProofScenario {
                scenario_id: "managed_launch".to_string(),
                performance_mode: "managed".to_string(),
                requested_memory_mb: Some(4096),
                version_id: Some("1.21.1".to_string()),
                benchmark_profile: Some("release-default".to_string()),
                benchmark_run_type: Some("repeat".to_string()),
                benchmark_mode: Some("release_validation".to_string()),
                benchmark_id: Some("release-default-repeat".to_string()),
            },
            device: crate::state::launch_reports::LaunchProofDevice {
                tier: "mid".to_string(),
                total_memory_mb: Some(16_384),
                cpu_threads: Some(8),
            },
            resource_budget: Some(family_c_resource_budget()),
            pid: Some(4242),
            exit_code: Some(1),
            boot_duration_ms: Some(3_250),
            priority: Some(crate::state::launch_reports::LaunchProofPriority {
                start_mode: "below_normal_until_boot".to_string(),
                start_error: Some(
                    "failed at /home/alice/.minecraft with -Xmx8192M token=priority-secret"
                        .to_string(),
                ),
                promotion: Some("promoted".to_string()),
                promotion_error: Some("account_id=priority-account".to_string()),
            }),
            failure_class: Some("jvm_unsupported_option".to_string()),
            failure_detail: Some(
                "Java failed in /home/alice/.minecraft with --accessToken raw-secret -Xmx8192M username=SecretPlayer account_id=abc provider_payload={} eyJheader123456789.eyJpayload123456789.signature123456789".to_string(),
            ),
            guardian: Some(json!({
                "mode": "managed",
                "decision": "warned",
                "message": "Guardian flagged launch settings for review.",
                "details": [
                    "Safe bounded guardian detail.",
                    "Leaked /Users/Alice/.minecraft --accessToken raw-secret -Xmx4096M username=SecretPlayer",
                    "Suspicious eyJheader123456789.eyJpayload123456789.signature123456789"
                ],
                "guidance": [
                    "Switch Guardian back to Managed if you want Axial to adjust unsafe choices.",
                    "provider_payload={\"token\":\"provider-secret\"}"
                ],
                "interventions": [
                    {
                        "kind": "strip_jvm_args",
                        "detail": "Explicit JVM args were removed before launch because they were incompatible."
                    },
                    {
                        "kind": "downgrade_preset",
                        "detail": "-XX:+UseZGC token=guardian-secret"
                    }
                ],
                "provider_payload": {
                    "access_token": "provider-secret"
                }
            })),
            healing: Some(json!({
                "requested_preset": "graalvm",
                "effective_preset": "performance",
                "auth_mode": "offline",
                "warnings": [
                    "Requested JVM preset was downgraded for compatibility.",
                    "Used C:\\Users\\Alice\\java.exe --username SecretPlayer -Dtoken=healing-secret"
                ],
                "fallback_applied": "/tmp/secret-fallback --accessToken healing-secret",
                "retry_count": 1,
                "failure_class": "jvm_unsupported_option",
                "events": [
                    {
                        "kind": "preset_downgraded",
                        "detail": "Requested JVM preset was downgraded for compatibility."
                    },
                    {
                        "kind": "fallback_applied",
                        "detail": "Fallback used /var/tmp/secret --accessToken healing-secret"
                    }
                ],
                "account_id": "healing-account"
            })),
            stages: vec![LaunchStageRecord {
                stage: "launching".to_string(),
                label: "Launching".to_string(),
                started_at_ms: 1_000,
                ended_at_ms: Some(1_250),
                duration_ms: Some(250),
                result: Some("completed".to_string()),
                warnings: vec![
                    "Cache warmed successfully.".to_string(),
                    "Bad stage path C:\\Users\\Alice\\.minecraft -Xmx8192M token=stage-secret"
                        .to_string(),
                ],
                fallback_reason: Some("Vanilla fallback selected.".to_string()),
                evidence: Vec::new(),
            }],
            comparison: Some(crate::state::launch_reports::LaunchProofComparison {
                baseline_session_id: "baseline-session".to_string(),
                baseline_recorded_at: "2026-01-01T00:00:00.000Z".to_string(),
                baseline: crate::state::launch_reports::LaunchProofComparisonBaseline {
                    performance_mode: "vanilla".to_string(),
                    version_id: "1.21.1".to_string(),
                    requested_memory_mb: Some(4096),
                    device_tier: "mid".to_string(),
                    benchmark_profile: Some("release-default".to_string()),
                    benchmark_run_type: Some("repeat".to_string()),
                    benchmark_mode: Some("release_validation".to_string()),
                },
                matched_sample_count: 2,
                metric_name: "boot_duration_ms".to_string(),
                current_value_ms: 3_250,
                baseline_value_ms: 4_000,
                delta_ms: -750,
                delta_percent: -18.75,
            }),
        }
}

fn assert_sanitized_launch_proof_payload(report: &serde_json::Value) {
    assert_eq!(report["session_id"], serde_json::json!("sensitive-proof"));
    assert_eq!(report["instance_id"], serde_json::json!("instance-safe"));
    assert_eq!(report["version_id"], serde_json::json!("1.21.1"));
    assert_eq!(report["outcome"], serde_json::json!("failed"));
    assert_eq!(
        report["scenario"]["performance_mode"],
        serde_json::json!("managed")
    );
    assert_eq!(
        report["scenario"]["requested_memory_mb"],
        serde_json::json!(4096)
    );
    assert_eq!(
        report["failure_class"],
        serde_json::json!("jvm_unsupported_option")
    );
    assert!(report.get("failure_detail").is_none());
    assert!(report.get("priority").is_none());
    assert_eq!(
        report["resource_budget"]["memory_headroom_mb"],
        serde_json::json!(2048)
    );
    assert_eq!(report["guardian"]["decision"], serde_json::json!("warned"));
    assert_eq!(
        report["view_model"],
        serde_json::json!({
            "outcome_label": "Failed",
            "outcome_tone": "err",
            "evidence": {
                "tone": "warn",
                "label": "Guardian warned",
                "detail": "Guardian flagged launch settings for review."
            },
            "comparison": {
                "label": "Boot faster by 750ms (18.8%)",
                "detail": "3.3s now, 4.0s baseline, 2 matched proofs",
                "tone": "ok"
            },
            "resource_budget": {
                "pressure_label": "Pressure clear",
                "details": [
                    "12 GB remaining",
                    "load 1.25/8 threads",
                    "64 GB disk free"
                ],
                "pressure": false
            },
        })
    );
    assert_eq!(
        report["guardian"]["details"][0],
        serde_json::json!("Safe bounded guardian detail.")
    );
    assert_eq!(report["healing"]["retry_count"], serde_json::json!(1));
    assert_eq!(
        report["healing"]["events"][0]["kind"],
        serde_json::json!("preset_downgraded")
    );
    assert_eq!(
        report["stages"][0]["warnings"][0],
        serde_json::json!("Cache warmed successfully.")
    );
    assert_eq!(
        report["stages"][0]["fallback_reason"],
        serde_json::json!("Vanilla fallback selected.")
    );
    assert_eq!(
        report["comparison"]["metric_name"],
        serde_json::json!("boot_duration_ms")
    );
    assert_eq!(report["comparison"]["delta_ms"], serde_json::json!(-750));
}

fn assert_launch_proof_payload_excludes_sensitive_content(data: &str) {
    for fragment in [
        "/home/alice",
        "/Users/Alice",
        "C:\\\\Users",
        "/tmp/secret-fallback",
        "/var/tmp/secret",
        "--accessToken",
        "-Xmx8192M",
        "-Xmx4096M",
        "-XX:+UseZGC",
        "-Dtoken",
        "SecretPlayer",
        "account_id",
        "provider_payload",
        "raw-secret",
        "priority-secret",
        "provider-secret",
        "healing-secret",
        "stage-secret",
        "java.exe",
        "eyJheader123456789",
    ] {
        assert!(
            !data.contains(fragment),
            "sanitized launch proof leaked fragment {fragment:?}: {data}"
        );
    }
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

    async fn reload(&self) -> Self {
        self.state
            .accounts()
            .close()
            .await
            .expect("close account store before reload");
        self.state
            .benchmark_suites()
            .close()
            .await
            .expect("close benchmark suite store before reload");
        self.state
            .benchmark_suite_drivers()
            .close()
            .await
            .expect("close benchmark suite driver store before reload");
        self.state
            .performance_operations()
            .close()
            .await
            .expect("close performance operation store before reload");
        self.state
            .journals()
            .close()
            .await
            .expect("close operation journal store before reload");
        self.state
            .failure_memory()
            .close()
            .await
            .expect("close failure memory store before reload");
        self.state
            .close_launch_reports()
            .await
            .expect("close launch report store before reload");
        Self::from_root_paths(self.root.clone(), self.paths.clone())
    }

    fn from_root_paths(root: PathBuf, paths: AppPaths) -> Self {
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
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

    fn configure_library(&self) {
        std::fs::create_dir_all(&self.paths.library_dir).expect("create library dir");
        let mut config = self.state.config().current();
        config.library_dir = self.paths.library_dir.to_string_lossy().to_string();
        self.state
            .config()
            .replace_in_memory(config)
            .expect("set library dir");
        self.state
            .set_library_dir(self.paths.library_dir.to_string_lossy().to_string());
    }

    fn set_launch_auth_mode(&self, mode: &str) {
        let mut config = self.state.config().current();
        config.launch_auth_mode = mode.to_string();
        self.state
            .config()
            .replace_in_memory(config)
            .expect("set launch auth mode");
    }

    fn write_ready_install(&self, version_id: &str) {
        self.write_version_json(
            version_id,
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                "libraries": []
            }),
        );
        let version_dir = self.paths.library_dir.join("versions").join(version_id);
        std::fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write client jar");
        self.write_ready_runtime("java-runtime-delta");
    }

    fn write_version_json(&self, version_id: &str, value: serde_json::Value) {
        let version_dir = self.paths.library_dir.join("versions").join(version_id);
        std::fs::create_dir_all(&version_dir).expect("version dir");
        std::fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec(&value).expect("version json"),
        )
        .expect("write version json");
    }

    fn write_ready_runtime(&self, component: &str) {
        let runtime_bin = self
            .paths
            .library_dir
            .join("runtime")
            .join(component)
            .join("bin");
        std::fs::create_dir_all(&runtime_bin).expect("runtime bin");
        let java_name = if cfg!(target_os = "windows") {
            "javaw.exe"
        } else {
            "java"
        };
        let java_path = runtime_bin.join(java_name);
        std::fs::write(&java_path, b"java").expect("runtime java");
        make_executable(&java_path);
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

    fn update_instance(&self, id: &str, update: impl FnOnce(&mut axial_config::Instance)) {
        let mut instance = self.state.instances().get(id).expect("instance");
        update(&mut instance);
        self.state
            .instances()
            .update(instance)
            .expect("update instance");
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
            .await
            .expect("active driver status persists");
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
            .await
            .expect("stopped driver status persists");
        self.state
            .benchmark_suite_drivers()
            .get(&started.status.id)
            .await
            .expect("stopped driver status")
    }

    async fn persist_suite_runs(&self, suite_id: &str, launched_run_indexes: &[usize]) {
        let plan = benchmark_suite_plan("development").expect("development plan");
        let manifest_runs = benchmark_suite_manifest_run_inputs("development", &plan);
        for run_index in launched_run_indexes {
            persist_suite_run(
                self.state.benchmark_suites(),
                suite_id,
                "instance",
                "development",
                &manifest_runs,
                *run_index,
                &format!("session-{run_index}"),
                "2026-01-01T00:00:00.000Z",
            )
            .await
            .expect("persist launched suite run");
        }
    }
}

fn test_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("axial-launch-{name}-{nanos}"))
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

fn test_suite_id(identity: &str, mode: &str) -> String {
    crate::state::benchmark_suites::derive_suite_id(identity, mode)
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("set executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

async fn persist_family_c_suite_run(
    state: &AppState,
    suite_id: &str,
    instance_id: &str,
    run_index: usize,
    session_id: &str,
) {
    let plan = benchmark_suite_plan("release_validation").expect("release plan");
    let manifest_runs = benchmark_suite_manifest_run_inputs("release_validation", &plan);
    persist_suite_run(
        state.benchmark_suites(),
        suite_id,
        instance_id,
        "release_validation",
        &manifest_runs,
        run_index,
        session_id,
        "2026-01-01T00:00:00.000Z",
    )
    .await
    .expect("persist launched family c suite run");
}

#[allow(clippy::too_many_arguments)]
async fn persist_suite_run(
    store: &crate::state::benchmark_suites::BenchmarkSuiteStore,
    suite_id: &str,
    instance_id: &str,
    mode: &str,
    plan: &[crate::state::benchmark_suites::BenchmarkSuiteRunInput],
    run_index: usize,
    session_id: &str,
    launched_at: &str,
) -> Result<
    crate::state::benchmark_suites::BenchmarkSuiteManifest,
    crate::state::benchmark_suites::BenchmarkSuiteReserveError,
> {
    let selection = store
        .select_reservation(suite_id, instance_id, mode, plan, Some(run_index))
        .await
        .map_err(crate::state::benchmark_suites::BenchmarkSuiteReserveError::PreAccept)?;
    store
        .reserve(selection, session_id, launched_at, false)
        .await
        .map(|reservation| reservation.manifest)
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
    fixture: &RouteTestFixture,
    run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
    instance_id: &str,
    performance_mode: &str,
    comparison: Option<crate::state::launch_reports::LaunchProofComparison>,
) {
    let proof = family_c_proof_record(run, instance_id, performance_mode, comparison);
    write_family_c_proof_record(fixture, &proof);
}

fn write_family_c_proof_record(
    fixture: &RouteTestFixture,
    proof: &crate::state::launch_reports::LaunchProofRecord,
) {
    fixture
        .state
        .launch_reports()
        .insert_unchecked_for_test(proof.clone());
}

fn write_family_c_managed_state(fixture: &RouteTestFixture, instance_id: &str) {
    let mods_dir = fixture.state.instances().game_dir(instance_id).join("mods");
    axial_performance::save_state(&mods_dir, &family_c_managed_state())
        .expect("write family c managed state");
}

fn write_invalid_family_c_managed_state(fixture: &RouteTestFixture, instance_id: &str) {
    let mods_dir = fixture.state.instances().game_dir(instance_id).join("mods");
    std::fs::create_dir_all(&mods_dir).expect("create mods dir");
    std::fs::write(
        axial_performance::state::lock_file_path(&mods_dir),
        "{ invalid",
    )
    .expect("write invalid managed state");
}

fn family_c_managed_state() -> axial_performance::CompositionState {
    axial_performance::CompositionState {
        composition_id: FAMILY_C_MANAGED_COMPOSITION_ID.to_string(),
        tier: axial_performance::CompositionTier::Core,
        installed_mods: vec![
            family_c_installed_mod("jupr7Bf5", "foamfix.jar"),
            family_c_installed_mod("DSVgwcji", "ai-improvements.jar"),
            family_c_installed_mod("clumps", "clumps.jar"),
        ],
        installed_at: "2026-01-01T00:00:00.000Z".to_string(),
        failure_count: 0,
        last_failure: String::new(),
    }
}

fn family_c_installed_mod(project_id: &str, filename: &str) -> axial_performance::InstalledMod {
    axial_performance::InstalledMod {
        project_id: project_id.to_string(),
        version_id: format!("{project_id}-version"),
        filename: filename.to_string(),
        ownership_class: axial_performance::OwnershipClass::CompositionManaged,
        source: axial_performance::ManagedArtifactSource {
            provider: axial_performance::ManagedArtifactProvider::Modrinth,
        },
        integrity: axial_performance::ManagedArtifactIntegrity {
            sha512: "a".repeat(128),
            sha512_verified: true,
        },
    }
}

fn family_c_comparison() -> crate::state::launch_reports::LaunchProofComparison {
    crate::state::launch_reports::LaunchProofComparison {
        baseline_session_id: "baseline-session".to_string(),
        baseline_recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
        baseline: family_c_comparison_baseline(),
        matched_sample_count: 1,
        metric_name: "total_completed_stage_duration_ms".to_string(),
        current_value_ms: 90,
        baseline_value_ms: 120,
        delta_ms: -30,
        delta_percent: -25.0,
    }
}

fn family_c_comparison_baseline() -> crate::state::launch_reports::LaunchProofComparisonBaseline {
    crate::state::launch_reports::LaunchProofComparisonBaseline {
        performance_mode: "vanilla".to_string(),
        version_id: "1.12.2".to_string(),
        requested_memory_mb: Some(4096),
        device_tier: "mid".to_string(),
        benchmark_profile: Some("vanilla_baseline".to_string()),
        benchmark_run_type: Some("coldish".to_string()),
        benchmark_mode: Some("release_validation".to_string()),
    }
}

fn family_c_proof_record(
    run: &crate::state::benchmark_suites::BenchmarkSuiteManifestRun,
    instance_id: &str,
    performance_mode: &str,
    comparison: Option<crate::state::launch_reports::LaunchProofComparison>,
) -> crate::state::launch_reports::LaunchProofRecord {
    let session_id = run.session_id.clone().expect("suite run session id");
    let scenario_id = match performance_mode {
        "vanilla" => "vanilla_launch",
        "managed" => "managed_launch",
        _ => "unknown_launch",
    };
    let launch_duration_ms = if performance_mode == "vanilla" {
        120
    } else {
        90
    };

    crate::state::launch_reports::LaunchProofRecord {
        schema: "axial.launch.proof".to_string(),
        schema_version: 2,
        session_id,
        instance_id: instance_id.to_string(),
        version_id: "1.12.2".to_string(),
        launched_at: "2026-01-01T00:00:00.000Z".to_string(),
        recorded_at: "2026-01-01T00:01:00.000Z".to_string(),
        outcome: "completed".to_string(),
        session_outcome: None,
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
        resource_budget: Some(family_c_resource_budget()),
        pid: None,
        exit_code: Some(0),
        boot_duration_ms: None,
        priority: None,
        failure_class: None,
        failure_detail: None,
        guardian: Some(json!({
            "mode": "managed",
            "decision": "allowed",
        })),
        healing: None,
        stages: vec![LaunchStageRecord {
            stage: "launching".to_string(),
            label: "Launching".to_string(),
            started_at_ms: 1_000,
            ended_at_ms: Some(1_000 + launch_duration_ms),
            duration_ms: Some(launch_duration_ms),
            result: Some("completed".to_string()),
            warnings: Vec::new(),
            fallback_reason: None,
            evidence: Vec::new(),
        }],
        comparison,
    }
}

fn family_c_resource_budget() -> crate::state::launch_reports::LaunchProofResourceBudget {
    crate::state::launch_reports::LaunchProofResourceBudget {
        host_total_memory_mb: Some(16_384),
        host_available_memory_mb: Some(12_288),
        host_used_memory_mb: Some(4_096),
        host_cpu_threads: Some(8),
        host_cpu_load_1m_x100: Some(125),
        host_cpu_load_5m_x100: Some(100),
        host_cpu_load_15m_x100: Some(75),
        launcher_process_memory_mb: Some(256),
        active_session_count: 0,
        active_install_count: 0,
        active_memory_allocation_mb: 0,
        requested_memory_mb: Some(4096),
        estimated_remaining_memory_mb: Some(12_288),
        memory_headroom_mb: 2048,
        memory_pressure: false,
        cpu_pressure: false,
        install_pressure: false,
        launch_disk_available_mb: Some(65_536),
        launch_disk_headroom_mb: axial_launcher::LAUNCH_DISK_HEADROOM_MB,
        disk_pressure: false,
    }
}

fn test_manifest(
    suite_id: &str,
    runs: Vec<crate::state::benchmark_suites::BenchmarkSuiteManifestRun>,
) -> crate::state::benchmark_suites::BenchmarkSuiteManifest {
    crate::state::benchmark_suites::BenchmarkSuiteManifest {
        schema: "axial.launch.benchmark.suite".to_string(),
        schema_version: 2,
        suite_id: suite_id.to_string(),
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
    let plan = benchmark_suite_plan("development").expect("development plan");
    let run = plan.get(run_index).copied().expect("planned run index");
    crate::state::benchmark_suites::BenchmarkSuiteManifestRun {
        run_index,
        profile: run.profile.to_string(),
        run_type: run.run_type.to_string(),
        target_id: run.target_id.unwrap_or_default().to_string(),
        benchmark_id: benchmark_suite_run_id("development", run_index, run),
        session_id: session_id.map(str::to_string),
        launched_at: session_id.map(|_| "2026-01-01T00:00:00.000Z".to_string()),
        state: if session_id.is_some() {
            "launching".to_string()
        } else {
            "pending".to_string()
        },
    }
}

fn launch_request_error(decision: Option<GuardianDecision>) -> launch_app::LaunchRequestError {
    launch_app::LaunchRequestError {
        message: "launch rejected".to_string(),
        healing: None,
        guardian: decision.map(|decision| axial_launcher::GuardianSummary {
            mode: axial_launcher::GuardianMode::Managed,
            decision,
            message: None,
            details: Vec::new(),
            guidance: Vec::new(),
            interventions: Vec::new(),
        }),
    }
}

fn raw_launch_control_io_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "failed to kill process 4242 while reading /home/alice/.axial/launch-reports/sensitive-proof.json from C:\\Users\\Alice\\AppData\\Roaming\\Axial\\launch-reports\\secret-report.json: Permission denied (os error 13)",
    )
}

fn raw_benchmark_suite_storage_io_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "failed to load benchmark suite manifest family-c-1-12-2 from /home/alice/.axial/benchmark-suites/family-c-1-12-2.json and C:\\Users\\Alice\\AppData\\Roaming\\Axial\\benchmark-suites\\suite-release_validation-00-family_c_forge.json: Permission denied (os error 13)",
    )
}

struct FailOnceBenchmarkSuiteBackend {
    failures: AtomicUsize,
}

impl FailOnceBenchmarkSuiteBackend {
    fn new() -> Self {
        Self {
            failures: AtomicUsize::new(1),
        }
    }
}

impl AtomicWriteBackend for FailOnceBenchmarkSuiteBackend {
    fn write(
        &self,
        target: &TargetDescriptor,
        destination: &Path,
        contents: &[u8],
    ) -> std::io::Result<()> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(raw_benchmark_suite_storage_io_error());
        }
        write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
            .map(|_| ())
            .map_err(std::io::Error::from)
    }
}

fn assert_public_error_excludes_raw_launch_control_fragments(
    response: (StatusCode, Json<serde_json::Value>),
    expected_status: StatusCode,
    expected_message: &str,
) {
    assert_eq!(response.0, expected_status);
    assert_eq!(
        response.1.0,
        serde_json::json!({ "error": expected_message })
    );
    let data = serde_json::to_string(&response.1.0).expect("serialize public error");
    for fragment in [
        "/home/alice",
        ".axial",
        "C:\\Users\\Alice",
        "AppData",
        "failed to kill",
        "process 4242",
        "Permission denied",
        "os error 13",
        "sensitive-proof.json",
        "secret-report.json",
        "launch-reports\\secret-report",
    ] {
        assert!(
            !data.contains(fragment),
            "public error leaked fragment {fragment:?}: {data}"
        );
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

fn test_launch_session_task() -> launch_app::LaunchSessionTask {
    launch_app::LaunchSessionTask {
        application: crate::application::stage_launch_instance_command(
            crate::application::LaunchInstanceCommand {
                instance_id: "instance-queued".to_string(),
                username: None,
                max_memory_mb: Some(6144),
                min_memory_mb: Some(1024),
                client_started_at_ms: None,
            },
            Some("session-queued".to_string()),
        ),
        boundary: crate::application::stage_launch_boundary(
            crate::application::LaunchBoundaryStagingRequest::new(
                crate::guardian::GuardianMode::Managed,
                crate::state::contracts::OperationPhase::Validating,
                &[],
                "managed",
            ),
        ),
        instance: Instance {
            id: "instance-queued".to_string(),
            name: "Queued Instance".to_string(),
            version_id: "1.21.1".to_string(),
            created_at: "2026-05-01T00:00:00Z".to_string(),
            last_played_at: String::new(),
            art_seed: 0,
            max_memory_mb: 6144,
            min_memory_mb: 1024,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: "managed".to_string(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
        },
        config: AppConfig::default(),
        intent: LaunchIntent {
            session_id: "session-queued".to_string(),
            library_dir: PathBuf::from("/tmp/axial-test-library"),
            instance_id: "instance-queued".to_string(),
            version_id: "1.21.1".to_string(),
            target_version_id: "1.21.1".to_string(),
            loader: "vanilla".to_string(),
            is_modded: false,
            username: "Player".to_string(),
            auth: LaunchAuthContext::offline("Player"),
            requested_java: String::new(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 6144,
            min_memory_mb: 1024,
            resolution: None,
            launcher_name: "axial".to_string(),
            launcher_version: "test".to_string(),
            game_dir: None,
            guardian: LaunchGuardianContext::default(),
            performance_mode: "managed".to_string(),
        },
        guardian: GuardianSummary::new(GuardianMode::Managed),
        launched_at: "2026-05-30T00:00:00Z".to_string(),
        benchmark: None,
        resource_budget: None,
    }
}
