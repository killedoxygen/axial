use crate::guardian::persisted_state_load_guardian_outcome;
use crate::state::AppState;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub status: &'static str,
    pub warnings: Vec<String>,
    pub library_dir: String,
    pub library_mode: String,
    pub setup_required: bool,
    pub app_name: String,
    pub version: String,
    pub dev_mode: bool,
}

pub fn launcher_status(state: &AppState) -> StatusResponse {
    let config = state.config().current();
    let library_dir = state.library_dir().unwrap_or_default();
    let mut warnings = state.startup_warnings();
    if let Some(outcome) =
        persisted_state_load_guardian_outcome(state.persisted_state_load_evidence())
    {
        warnings.push(outcome.user_outcome.summary().to_string());
    }

    StatusResponse {
        status: "ok",
        warnings,
        setup_required: library_dir.is_empty(),
        library_dir,
        library_mode: config.library_mode,
        app_name: state.app_name().to_string(),
        version: state.version().to_string(),
        dev_mode: cfg!(debug_assertions),
    }
}

#[cfg(test)]
mod tests {
    use super::launcher_status;
    use crate::state::performance_operations::{operation_dir, operation_path};
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[test]
    fn status_includes_startup_warnings_and_remains_ok() {
        let root = test_root("status-startup-warnings");
        let paths = test_paths(&root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: vec!["startup warning".to_string()],
            frontend_dir: root.join("frontend"),
        });

        let response = launcher_status(&state);

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings, vec!["startup warning".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_aggregates_five_stores_and_owns_two_store_rejection_evidence() {
        let root = test_root("status-operation-state-warning");
        let paths = test_paths(&root);
        let operation_dir = operation_dir(&paths);
        fs::create_dir_all(&operation_dir).expect("create operation dir");
        fs::write(
            operation_path(
                &operation_dir,
                "performance-install-00000000000000000000000000000001",
            ),
            serde_json::to_vec(&serde_json::json!({
                "id": "performance-install-00000000000000000000000000000001",
                "instance_id": "instance-a",
                "action": "install",
                "payload": {
                    "unexpected_mode": true
                },
                "state": "applying",
                "error": null,
                "created_at": "2026-01-01T00:00:00.000Z",
                "updated_at": "2026-01-01T00:01:00.000Z"
            }))
            .expect("serialize status"),
        )
        .expect("write status");
        let driver_id = "benchmark-suite-driver-0000000000000001";
        let driver_dir = paths.config_dir.join("benchmarks").join("suite-drivers");
        fs::create_dir_all(&driver_dir).expect("create driver dir");
        fs::write(driver_dir.join(format!("{driver_id}.json")), b"{")
            .expect("write malformed driver");
        let suite_id =
            crate::state::benchmark_suites::derive_suite_id("status-aggregate", "development");
        let suite_path = crate::state::benchmark_suites::suite_path(&paths, &suite_id);
        fs::create_dir_all(suite_path.parent().expect("suite directory"))
            .expect("create suite directory");
        fs::write(suite_path, b"{").expect("write malformed suite");

        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });

        let response = launcher_status(&state);
        let load_evidence = state.persisted_state_load_evidence();

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(
            response.warnings[0],
            "Guardian kept Axial running after persisted operation state could not be trusted."
        );
        assert!(!response.warnings[0].contains(&root.to_string_lossy().to_string()));
        assert!(!response.warnings[0].contains("unexpected_mode"));
        assert!(!response.warnings[0].contains("line"));
        assert_eq!(load_evidence.issue_count(), 3);
        assert_eq!(load_evidence.rejected_records().len(), 2);
        assert_eq!(
            load_evidence.rejected_records()[0].target().id,
            "performance-install-00000000000000000000000000000001"
        );
        assert_eq!(load_evidence.rejected_records()[1].target().id, driver_id);
        assert_eq!(
            format!("{:?}", load_evidence.rejected_records()[0].store()),
            "PerformanceOperation"
        );
        assert_eq!(
            format!("{:?}", load_evidence.rejected_records()[1].store()),
            "BenchmarkSuiteDriver"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_includes_guardian_warning_for_rejected_benchmark_suite_manifest() {
        let root = test_root("status-benchmark-suite-warning");
        let paths = test_paths(&root);
        let suite_id =
            crate::state::benchmark_suites::derive_suite_id("status-rejected-suite", "development");
        let suite_path = crate::state::benchmark_suites::suite_path(&paths, &suite_id);
        fs::create_dir_all(suite_path.parent().expect("suite directory"))
            .expect("create suite directory");
        fs::write(&suite_path, b"{not-json").expect("write malformed suite");

        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });

        let response = launcher_status(&state);

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(
            response.warnings[0],
            "Guardian kept Axial running after persisted operation state could not be trusted."
        );
        assert!(!response.warnings[0].contains(&root.to_string_lossy().to_string()));
        assert!(!response.warnings[0].contains("not-json"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_includes_guardian_warning_for_rejected_launch_report() {
        let root = test_root("status-launch-report-warning");
        let paths = test_paths(&root);
        let report_path = paths
            .config_dir
            .join("benchmarks")
            .join("launch")
            .join("rejected-report.json");
        fs::create_dir_all(report_path.parent().expect("report directory"))
            .expect("create report directory");
        fs::write(&report_path, b"{not-json").expect("write malformed report");

        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });

        let response = launcher_status(&state);

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(
            response.warnings[0],
            "Guardian kept Axial running after persisted operation state could not be trusted."
        );
        assert!(!response.warnings[0].contains(&root.to_string_lossy().to_string()));
        assert!(!response.warnings[0].contains("not-json"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_includes_instance_registry_startup_warning_and_remains_ok() {
        let root = test_root("status-instance-startup-warning");
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.config_dir).expect("create config dir");
        fs::write(&paths.instances_file, "{not valid json").expect("write malformed registry");

        let config_startup =
            ConfigStore::load_for_startup(paths.clone()).expect("load config for startup");
        let instance_startup = InstanceStore::load_for_startup(paths.clone());
        let mut startup_warnings = config_startup.warnings;
        startup_warnings.extend(instance_startup.warnings);
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config: Arc::new(config_startup.store),
            instances: Arc::new(instance_startup.store),
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings,
            frontend_dir: root.join("frontend"),
        });

        let response = launcher_status(&state);

        assert_eq!(response.status, "ok");
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(
            response.warnings[0],
            "Axial could not load the instance list, so it started with an empty list. Check app data permissions or restore the instance registry."
        );
        assert!(!response.warnings[0].contains(&root.to_string_lossy().to_string()));
        assert!(!response.warnings[0].contains("expected"));
        assert!(!response.warnings[0].contains("line"));

        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-api-status-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
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
