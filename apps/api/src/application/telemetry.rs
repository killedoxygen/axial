use crate::{
    observability::telemetry::{
        MAX_EXCEPTION_SUMMARY_CHARS, TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel,
        TelemetryEvent,
    },
    state::AppState,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrontendErrorReportRequest {
    pub kind: String,
    pub name: String,
    pub message: String,
}

pub fn report_frontend_error(
    state: &AppState,
    request: FrontendErrorReportRequest,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    validate_frontend_error_kind(&request.kind)?;

    state.telemetry().emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::FrontendError,
        TelemetryErrorArea::Frontend,
        TelemetryErrorLevel::Error,
        frontend_error_summary(&request.name, &request.message),
    ));

    Ok(())
}

fn validate_frontend_error_kind(kind: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if matches!(kind, "error" | "unhandledrejection" | "render") {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid error kind" })),
        ))
    }
}

fn frontend_error_summary(name: &str, message: &str) -> String {
    truncate_chars(
        &format!("{}: {}", name.trim(), message.trim()),
        MAX_EXCEPTION_SUMMARY_CHARS,
    )
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::telemetry::{DEFAULT_POSTHOG_HOST, TelemetryHub};
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::{fs, path::PathBuf, sync::Arc};

    const TEST_KEY: &str = "phc_test";
    const TEST_INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

    #[test]
    fn valid_frontend_error_queues_posthog_exception() {
        let fixture = TestFixture::new("valid");

        report_frontend_error(
            &fixture.state,
            FrontendErrorReportRequest {
                kind: "error".to_string(),
                name: "TypeError".to_string(),
                message: "Cannot read launcher state".to_string(),
            },
        )
        .expect("valid frontend error should report");

        let queued = fixture.telemetry.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["event"], "$exception");
        assert_eq!(queued[0]["properties"]["area"], "frontend");
        assert_eq!(
            queued[0]["properties"]["$exception_fingerprint"],
            "frontend_error"
        );
        assert_eq!(queued[0]["properties"]["$exception_level"], "error");
        assert_eq!(
            queued[0]["properties"]["$exception_list"][0]["type"],
            "frontend_error"
        );
        assert_eq!(
            queued[0]["properties"]["$exception_list"][0]["value"],
            "TypeError: Cannot read launcher state"
        );
    }

    #[test]
    fn invalid_frontend_error_kind_returns_400() {
        let fixture = TestFixture::new("invalid-kind");

        let error = report_frontend_error(
            &fixture.state,
            FrontendErrorReportRequest {
                kind: "stack".to_string(),
                name: "Error".to_string(),
                message: "ignored".to_string(),
            },
        )
        .expect_err("invalid frontend error kind should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1.0, json!({ "error": "invalid error kind" }));
        assert_eq!(fixture.telemetry.queue_len_for_test(), 0);
    }

    #[test]
    fn frontend_error_summary_is_bounded_and_sanitized_by_hub() {
        let fixture = TestFixture::new("sanitized");

        report_frontend_error(
            &fixture.state,
            FrontendErrorReportRequest {
                kind: "render".to_string(),
                name: "RenderError".to_string(),
                message: "failed rendering /Users/alice/Axial/private-instance/config.json"
                    .to_string(),
            },
        )
        .expect("path-like frontend error should still queue redacted exception");

        let queued = fixture.telemetry.queued_batch_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0]["properties"]["$exception_list"][0]["value"],
            "[redacted]"
        );
    }

    struct TestFixture {
        root: PathBuf,
        state: AppState,
        telemetry: Arc<TelemetryHub>,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    telemetry_enabled: true,
                    telemetry_install_id: TEST_INSTALL_ID.to_string(),
                    ..AppConfig::default()
                },
            )
            .expect("seed config");
            let config = Arc::new(config);
            let telemetry = Arc::new(TelemetryHub::new(
                config.clone(),
                Some(TEST_KEY.to_string()),
                DEFAULT_POSTHOG_HOST.to_string(),
            ));
            let state = AppState::new_with_telemetry(
                AppStateInit {
                    app_name: "Axial".to_string(),
                    version: "test".to_string(),
                    instances: Arc::new(
                        InstanceStore::from_snapshot(
                            paths.clone(),
                            InstanceRegistrySnapshot::default(),
                        )
                        .expect("load instances"),
                    ),
                    installs: Arc::new(InstallStore::new()),
                    sessions: Arc::new(SessionStore::new()),
                    performance: Arc::new(
                        PerformanceManager::load_for_startup(&paths.config_dir)
                            .expect("performance manager"),
                    ),
                    config,
                    startup_warnings: Vec::new(),
                },
                telemetry.clone(),
            );

            Self {
                root,
                state,
                telemetry,
            }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-frontend-telemetry-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
