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
        Ok(_) => {
            trace_launch_event(session_id, "launch proof persisted");
            if let Err(error) = crate::state::benchmark_suites::update_run_state_for_session(
                state.config().paths(),
                session_id,
                outcome,
            ) {
                trace_launch_event(
                    session_id,
                    &format!("benchmark suite manifest state update failed: {error}"),
                );
            }
        }
        Err(error) => trace_launch_event(
            session_id,
            &format!("launch proof persistence failed: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

        let suite_id = "launch-proof-persistence-suite";
        let plan = vec![crate::state::benchmark_suites::BenchmarkSuiteRunInput {
            run_index: 0,
            profile: "managed_default".to_string(),
            run_type: "coldish".to_string(),
            target_id: Some("managed_default".to_string()),
            benchmark_id: "launch-proof-benchmark".to_string(),
        }];
        crate::state::benchmark_suites::persist_launched_run(
            state.config().paths(),
            suite_id,
            "instance",
            "development",
            &plan,
            0,
            session_id,
            "2026-01-01T00:00:00.000Z",
        )
        .expect("persist benchmark suite run");

        persist_launch_proof_best_effort(&state, session_id, None, "running").await;

        let proof = crate::state::launch_reports::load(state.config().paths(), session_id)
            .expect("load proof")
            .expect("proof exists");
        let proof_json = serde_json::to_string(&proof).expect("proof json");
        assert!(proof_json.contains("execution_launch_command_prepared"));
        assert!(proof_json.contains("arg_count:3"));
        assert_no_sensitive_stage_evidence(&proof_json);

        let manifest = crate::state::benchmark_suites::load(state.config().paths(), suite_id)
            .expect("load suite")
            .expect("suite exists");
        assert_eq!(manifest.runs[0].session_id.as_deref(), Some(session_id));
        assert_eq!(manifest.runs[0].state, "running");

        let _ = fs::remove_dir_all(root);
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
