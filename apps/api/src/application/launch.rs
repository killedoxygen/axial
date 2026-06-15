//! Application-owned launch command staging.
//!
//! This module owns the Application command identity for launch workflows. It
//! does not perform route extraction, core launch preparation, process spawning,
//! or self-healing.

use super::{
    ApplicationCommand, ApplicationCommandRequest, CommandResult, CommandResultCarriers,
    LaunchInstanceCommand, LaunchInstancePayload, SessionCommandCarrier,
};
use crate::guardian::{
    GuardianDecision, GuardianDecisionKind, GuardianFact, GuardianMode, GuardianPolicyContext,
    SafetyCase, SafetyOutcome, build_safety_case, decide_guardian_policy,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text, sanitize_evidence_token};
use crate::state::contracts::{CommandKind, OperationId, OperationPhase, OperationStatus};
use croopor_launcher::LaunchStageEvidence;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchInstanceStaging {
    pub command: ApplicationCommand,
    pub result: CommandResult<LaunchInstancePayload>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchBoundaryStaging {
    pub safety_case: SafetyCase,
    pub guardian_decision: GuardianDecision,
    pub safety: SafetyOutcome,
    pub performance_mode: String,
}

#[derive(Clone, Debug)]
pub struct LaunchBoundaryStagingRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub guardian_mode: GuardianMode,
    pub phase: OperationPhase,
    pub guardian_facts: &'a [GuardianFact],
    pub performance_mode: &'a str,
}

impl<'a> LaunchBoundaryStagingRequest<'a> {
    pub fn new(
        guardian_mode: GuardianMode,
        phase: OperationPhase,
        guardian_facts: &'a [GuardianFact],
        performance_mode: &'a str,
    ) -> Self {
        Self {
            operation_id: None,
            guardian_mode,
            phase,
            guardian_facts,
            performance_mode,
        }
    }
}

pub fn stage_launch_instance_command(
    request: LaunchInstanceCommand,
    session_id: Option<String>,
) -> LaunchInstanceStaging {
    let command = ApplicationCommandRequest::LaunchInstance(request).command();
    let result = CommandResult {
        command: CommandKind::LaunchInstance,
        operation_id: None,
        status: OperationStatus::Planned,
        safety: None,
        carriers: CommandResultCarriers {
            session: Some(SessionCommandCarrier {
                session_id: session_id.clone(),
                state: session_id.as_ref().map(|_| "queued".to_string()),
                pid: None,
                exit_code: None,
            }),
            ..CommandResultCarriers::default()
        },
        payload: LaunchInstancePayload {
            session_id,
            operation_id: None,
        },
        view_model: None,
    };

    LaunchInstanceStaging { command, result }
}

pub fn stage_launch_boundary(request: LaunchBoundaryStagingRequest<'_>) -> LaunchBoundaryStaging {
    let safety_case = build_safety_case(
        request.operation_id,
        request.guardian_mode,
        request.phase,
        request.guardian_facts,
    );
    let guardian_decision =
        decide_guardian_policy(&safety_case, GuardianPolicyContext::current_operation());
    let safety = launch_boundary_safety_outcome(&guardian_decision, &safety_case);
    let performance_mode =
        sanitize_evidence_token(request.performance_mode, RedactionAudience::UserVisible, 64)
            .unwrap_or_else(|| "unknown".to_string());

    LaunchBoundaryStaging {
        safety_case,
        guardian_decision,
        safety,
        performance_mode,
    }
}

pub fn launch_application_stage_evidence(
    staging: &LaunchInstanceStaging,
) -> Vec<LaunchStageEvidence> {
    let mut details = vec!["command:launch_instance".to_string()];
    if let Some(session_state) = staging
        .result
        .carriers
        .session
        .as_ref()
        .and_then(|session| session.state.as_deref())
        .and_then(|state| sanitize_evidence_token(state, RedactionAudience::UserVisible, 32))
    {
        details.push(format!("session_state:{session_state}"));
    }
    details.push(format!("status:{:?}", staging.result.status));

    vec![launch_stage_evidence(
        "application_launch_command_staged",
        "application",
        "Application staged the launch command.",
        details,
    )]
}

pub fn launch_boundary_stage_evidence(staging: &LaunchBoundaryStaging) -> Vec<LaunchStageEvidence> {
    vec![
        launch_stage_evidence(
            "guardian_launch_safety_decision",
            "guardian",
            "Guardian recorded the launch safety decision.",
            vec![
                format!("mode:{:?}", staging.guardian_decision.mode),
                format!("decision:{:?}", staging.guardian_decision.kind),
                format!("diagnoses:{}", staging.safety_case.diagnoses.len()),
            ],
        ),
        launch_stage_evidence(
            "performance_launch_plan_input",
            "performance",
            "Performance launch inputs were recorded.",
            vec![format!("mode:{}", staging.performance_mode)],
        ),
    ]
}

fn launch_boundary_safety_outcome(
    decision: &GuardianDecision,
    safety_case: &SafetyCase,
) -> SafetyOutcome {
    SafetyOutcome {
        decision: decision.kind,
        summary: launch_boundary_safety_summary(decision.kind).to_string(),
        detail: safety_case
            .diagnoses
            .first()
            .map(|diagnosis| diagnosis.public_reason_template.clone()),
        diagnoses: decision.diagnoses.clone(),
    }
}

fn launch_boundary_safety_summary(decision: GuardianDecisionKind) -> &'static str {
    match decision {
        GuardianDecisionKind::Allow | GuardianDecisionKind::RecordOnly => {
            "Launch safety checks are recorded."
        }
        GuardianDecisionKind::Warn => "Launch safety checks produced warnings.",
        GuardianDecisionKind::Block => "Launch safety checks blocked the command.",
        GuardianDecisionKind::AskUser => "Launch safety checks require user confirmation.",
        _ => "Launch safety checks selected a guarded action.",
    }
}

fn launch_stage_evidence(
    id: &str,
    system: &str,
    summary: &str,
    details: Vec<String>,
) -> LaunchStageEvidence {
    LaunchStageEvidence {
        id: sanitize_evidence_token(id, RedactionAudience::UserVisible, 64)
            .unwrap_or_else(|| "launch_stage_evidence".to_string()),
        system: sanitize_evidence_token(system, RedactionAudience::UserVisible, 32)
            .unwrap_or_else(|| "application".to_string()),
        summary: sanitize_evidence_text(summary, RedactionAudience::UserVisible, 160)
            .unwrap_or_else(|| "Launch stage evidence recorded.".to_string()),
        details: details
            .into_iter()
            .filter_map(|detail| {
                sanitize_evidence_text(&detail, RedactionAudience::UserVisible, 120)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::stage_launch_instance_command;
    use crate::application::LaunchInstanceCommand;
    use crate::execution::runtime::runtime_fact;
    use crate::guardian::guardian_fact_from_execution;
    use crate::state::contracts::{
        CommandKind, OperationPhase, OperationStatus, OwnershipClass, StabilizationSystem,
        TargetDescriptor, TargetKind,
    };
    use crate::{application::LaunchBoundaryStagingRequest, execution::ExecutionFactKind};

    #[test]
    fn launch_staging_builds_application_command_and_session_carrier() {
        let staging = stage_launch_instance_command(
            LaunchInstanceCommand {
                instance_id: "instance-1".to_string(),
                username: Some("Player".to_string()),
                max_memory_mb: Some(4096),
                min_memory_mb: None,
                client_started_at_ms: Some(42),
            },
            Some("session-1".to_string()),
        );

        assert_eq!(staging.command.kind, CommandKind::LaunchInstance);
        assert_eq!(
            staging.command.target.as_ref().map(|target| target.kind),
            Some(TargetKind::Instance)
        );
        assert_eq!(staging.result.status, OperationStatus::Planned);
        assert_eq!(
            staging.result.payload.session_id.as_deref(),
            Some("session-1")
        );
        assert_eq!(
            staging
                .result
                .carriers
                .session
                .as_ref()
                .and_then(|session| session.state.as_deref()),
            Some("queued")
        );
    }

    #[test]
    fn launch_boundary_staging_authors_safety_case_and_sanitized_performance_mode() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Runtime,
            "manual_java",
            OwnershipClass::UserOwned,
        );
        let execution_fact = runtime_fact(
            ExecutionFactKind::RuntimeMissingExecutable,
            None,
            &target,
            Vec::new(),
        );
        let guardian_fact =
            guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);

        let staging = super::stage_launch_boundary(LaunchBoundaryStagingRequest::new(
            crate::guardian::GuardianMode::Managed,
            OperationPhase::Validating,
            &[guardian_fact],
            "managed C:\\Users\\Alice",
        ));

        assert_eq!(staging.safety_case.diagnoses.len(), 1);
        assert_eq!(
            staging.guardian_decision.kind,
            crate::guardian::GuardianDecisionKind::Fallback
        );
        assert_eq!(
            staging.safety.diagnoses[0].as_str(),
            "java_override_unavailable"
        );
        assert_eq!(staging.performance_mode, "unknown");
    }

    #[test]
    fn launch_stage_evidence_redacts_boundary_inputs() {
        let staging = super::stage_launch_boundary(LaunchBoundaryStagingRequest::new(
            crate::guardian::GuardianMode::Managed,
            OperationPhase::Validating,
            &[],
            r"managed C:\Users\Alice -Xmx8192M",
        ));
        let evidence = super::launch_boundary_stage_evidence(&staging);
        let encoded = serde_json::to_string(&evidence).expect("stage evidence json");

        assert!(encoded.contains("guardian_launch_safety_decision"));
        assert!(encoded.contains("performance_launch_plan_input"));
        assert!(encoded.contains("mode:unknown"));
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("-Xmx"));
        assert!(!encoded.contains("Users"));
    }
}
