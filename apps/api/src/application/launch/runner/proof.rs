use super::trace_launch_event;
use crate::state::AppState;
use crate::state::launch_reports::LaunchProofContext;

pub async fn persist_launch_proof_best_effort(
    state: &AppState,
    session_id: &str,
    launched_at: Option<&str>,
    outcome: &str,
) {
    persist_launch_proof_best_effort_with_context(state, session_id, launched_at, outcome, None)
        .await;
}

pub(super) async fn persist_launch_proof_best_effort_with_context(
    state: &AppState,
    session_id: &str,
    launched_at: Option<&str>,
    outcome: &str,
    proof_context: Option<&LaunchProofContext>,
) {
    let Some(record) = state.sessions().get(session_id).await else {
        trace_launch_event(session_id, "launch proof skipped: session record missing");
        return;
    };
    match crate::state::launch_reports::persist_record_with_context(
        state.config().paths(),
        &record,
        launched_at,
        outcome,
        proof_context,
    ) {
        Ok(_) => trace_launch_event(session_id, "launch proof persisted"),
        Err(_) => trace_launch_event(session_id, "launch proof persistence failed"),
    }
    if let Err(error) = state
        .benchmark_suites()
        .update_run_state_for_session(session_id, outcome)
        .await
    {
        tracing::warn!(
            error_class = error.class(),
            "benchmark suite outcome persistence failed"
        );
        trace_launch_event(session_id, "benchmark suite outcome persistence failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::performance::{
        benchmark_suite_manifest_run_inputs, benchmark_suite_plan,
    };
    use crate::execution::launch::{
        LaunchCommandPreparationRequest, launch_command_stage_evidence, prepare_launch_command,
    };
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceStore};
    use axial_launcher::{LaunchSessionRecord, LaunchState, SessionId};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn persist_launch_proof_best_effort_records_stage_evidence_and_updates_benchmark_run() {
        let root = unique_test_dir("launch-proof-persistence");
        let state = test_app_state(&root);
        let session_id = "launch-proof-persistence";
        state.sessions().insert(test_record(session_id)).await;

        let command = vec![
            r"C:\Users\Alice\.jdks\java.exe".to_string(),
            "-cp".to_string(),
            "libraries".to_string(),
        ];
        let prepared = prepare_launch_command(LaunchCommandPreparationRequest::new(
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Session,
                session_id,
                OwnershipClass::LauncherManaged,
            ),
            &command,
            &root,
        ))
        .expect("prepared command");
        state
            .sessions()
            .record_stage_evidence(session_id, launch_command_stage_evidence(&prepared.facts))
            .await;

        let instance_id = "instance";
        let suite_id = crate::state::benchmark_suites::derive_suite_id(instance_id, "development");
        let plan = development_suite_plan();
        let selection = state
            .benchmark_suites()
            .select_reservation(&suite_id, instance_id, "development", &plan, Some(0))
            .await
            .expect("select benchmark suite run");
        state
            .benchmark_suites()
            .reserve(selection, session_id, "2026-01-01T00:00:00.000Z", false)
            .await
            .expect("persist benchmark suite run");

        persist_launch_proof_best_effort(&state, session_id, None, "running").await;

        let proof = crate::state::launch_reports::load(state.config().paths(), session_id)
            .expect("load proof")
            .expect("proof exists");
        let proof_json = serde_json::to_string(&proof).expect("proof json");
        assert!(proof_json.contains("execution_launch_command_prepared"));
        assert!(proof_json.contains("arg_count:3"));
        assert_no_sensitive_stage_evidence(&proof_json);

        let manifest = state
            .benchmark_suites()
            .get(&suite_id)
            .expect("load suite")
            .expect("suite exists");
        assert_eq!(manifest.runs[0].session_id.as_deref(), Some(session_id));
        assert_eq!(manifest.runs[0].state, "running");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn benchmark_outcome_commits_even_when_launch_proof_write_fails() {
        let root = unique_test_dir("launch-proof-failure-suite-outcome");
        let state = test_app_state(&root);
        let session_id = "launch-proof-failure-suite-outcome";
        state.sessions().insert(test_record(session_id)).await;
        let instance_id = "instance";
        let suite_id = crate::state::benchmark_suites::derive_suite_id(instance_id, "development");
        let plan = development_suite_plan();
        let selection = state
            .benchmark_suites()
            .select_reservation(&suite_id, instance_id, "development", &plan, Some(0))
            .await
            .expect("select benchmark suite run");
        state
            .benchmark_suites()
            .reserve(selection, session_id, "2026-01-01T00:00:00.000Z", false)
            .await
            .expect("persist benchmark suite run");
        let report_dir = state
            .config()
            .paths()
            .config_dir
            .join("benchmarks")
            .join("launch");
        fs::create_dir_all(report_dir.parent().expect("report parent"))
            .expect("create report parent");
        fs::write(&report_dir, b"not a directory").expect("block report directory");

        persist_launch_proof_best_effort(&state, session_id, None, "running").await;

        let manifest = state
            .benchmark_suites()
            .get(&suite_id)
            .expect("read committed suite")
            .expect("suite exists");
        assert_eq!(manifest.runs[0].state, "running");

        let _ = fs::remove_dir_all(root);
    }

    fn development_suite_plan() -> Vec<crate::state::benchmark_suites::BenchmarkSuiteRunInput> {
        let plan = benchmark_suite_plan("development").expect("development benchmark suite plan");
        benchmark_suite_manifest_run_inputs("development", &plan)
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(PerformanceManager::new().expect("performance manager")),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
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

    fn assert_no_sensitive_stage_evidence(text: &str) {
        for fragment in [
            "/home/alice",
            "/home/",
            "C:\\Users",
            "Alice",
            ".jdks",
            ".minecraft",
            "java.exe",
            "--accessToken",
            "-Xmx",
            "-cp",
            "token",
            "SecretPlayer",
        ] {
            assert!(
                !text.contains(fragment),
                "stage evidence leaked fragment {fragment:?}: {text}"
            );
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
