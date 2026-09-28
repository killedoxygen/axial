use crate::execution::process::{
    ProcessSpawnRequest, process_session_target, process_spawn_failed_stage_evidence,
    process_spawned, process_stage_evidence,
};
use crate::state::contracts::TargetDescriptor;

pub(super) fn launch_command_target(session_id: &str) -> TargetDescriptor {
    process_session_target(session_id)
}

pub(super) fn launch_spawn_stage_evidence(
    session_id: &str,
    record: &crate::state::LaunchSessionRecord,
) -> Vec<axial_launcher::LaunchStageEvidence> {
    let Some(pid) = record.pid else {
        return launch_spawn_failed_stage_evidence();
    };
    match process_spawned(
        ProcessSpawnRequest::new(launch_command_target(session_id))
            .with_command_label("game_session"),
        pid,
    ) {
        Ok(report) => process_stage_evidence(&report.facts),
        Err(error) => process_stage_evidence(&error.facts),
    }
}

pub(super) fn launch_spawn_failed_stage_evidence() -> Vec<axial_launcher::LaunchStageEvidence> {
    vec![process_spawn_failed_stage_evidence()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetKind};
    use axial_launcher::{LaunchSessionRecord, LaunchState, SessionId};

    #[test]
    fn launch_command_target_uses_execution_session_target() {
        let target = launch_command_target("session-1");

        assert_eq!(target.system, StabilizationSystem::Execution);
        assert_eq!(target.kind, TargetKind::Session);
        assert_eq!(target.id, "session-1");
        assert_eq!(target.ownership, OwnershipClass::LauncherManaged);
    }

    #[test]
    fn launch_spawn_stage_evidence_records_spawned_game_session() {
        let record = test_record("session-1", Some(4242));

        let evidence = launch_spawn_stage_evidence("session-1", &record);
        let encoded = serde_json::to_string(&evidence).expect("serialize evidence");

        assert!(encoded.contains("execution_process_spawned"));
        assert!(encoded.contains("process_kind"));
        assert!(encoded.contains("game_session"));
        assert!(encoded.contains("command"));
        assert!(encoded.contains("game_session"));
    }

    #[test]
    fn launch_spawn_stage_evidence_falls_back_when_pid_missing() {
        let record = test_record("session-1", None);

        let evidence = launch_spawn_stage_evidence("session-1", &record);

        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].id, "execution_process_spawn_failed");
    }

    fn test_record(session_id: &str, pid: Option<u32>) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Starting,
            pid,
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
}
