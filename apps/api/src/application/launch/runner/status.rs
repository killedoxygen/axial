use crate::guardian::GuardianSummary;
use crate::state::{AppState, LaunchStatusEvent};
use axial_launcher::{
    LaunchFailureClass, LaunchPreparationEvent, LaunchState, failure_class_name, launch_state_name,
};
use serde_json::Value;

pub(super) fn launch_state_for_preparation_event(event: LaunchPreparationEvent) -> LaunchState {
    match event {
        LaunchPreparationEvent::Planning => LaunchState::Planning,
        LaunchPreparationEvent::EnsuringRuntime => LaunchState::EnsuringRuntime,
        LaunchPreparationEvent::DownloadingRuntime => LaunchState::DownloadingRuntime,
        LaunchPreparationEvent::Validating => LaunchState::Validating,
        LaunchPreparationEvent::Preparing => LaunchState::Preparing,
    }
}

pub(super) async fn emit_status(
    state: &AppState,
    session_id: &str,
    launch_state: LaunchState,
    pid: Option<u32>,
    failure_class: Option<LaunchFailureClass>,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) {
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: launch_state_name(launch_state).to_string(),
                benchmark: None,
                pid,
                exit_code: None,
                failure_class: failure_class.map(failure_class_name).map(str::to_string),
                failure_detail: None,
                crash_evidence: None,
                healing: serialize_healing(healing),
                guardian: serialize_guardian(guardian),
                outcome: None,
                notice: None,
                evidence: Vec::new(),
                stages: Vec::new(),
            },
        )
        .await;
}

pub(super) fn serialize_healing(
    healing: Option<axial_launcher::LaunchHealingSummary>,
) -> Option<Value> {
    healing.and_then(|value| serde_json::to_value(value).ok())
}

pub(super) fn serialize_guardian(guardian: Option<GuardianSummary>) -> Option<Value> {
    guardian.and_then(|value| serde_json::to_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_events_map_to_existing_launch_states() {
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Planning),
            LaunchState::Planning
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::EnsuringRuntime),
            LaunchState::EnsuringRuntime
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::DownloadingRuntime),
            LaunchState::DownloadingRuntime
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Validating),
            LaunchState::Validating
        );
        assert_eq!(
            launch_state_for_preparation_event(LaunchPreparationEvent::Preparing),
            LaunchState::Preparing
        );
    }
}
