//! Execution-owned process capability contracts.
//!
//! These helpers emit primitive process facts. They do not classify exits as
//! user-facing crashes or clean exits.

use super::{ExecutionFact, ExecutionFactKind, execution_fact_stage_evidence};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_text,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use croopor_launcher::LaunchStageEvidence;
use std::fmt;

#[derive(Clone, Debug)]
pub struct ProcessSpawnRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub command_label: Option<&'a str>,
    pub process_kind: ProcessKind,
}

impl<'a> ProcessSpawnRequest<'a> {
    pub fn new(target: TargetDescriptor) -> Self {
        Self {
            operation_id: None,
            target,
            command_label: None,
            process_kind: ProcessKind::GameSession,
        }
    }

    pub fn with_command_label(mut self, command_label: &'a str) -> Self {
        self.command_label = Some(command_label);
        self
    }

    pub fn with_process_kind(mut self, process_kind: ProcessKind) -> Self {
        self.process_kind = process_kind;
        self
    }
}

#[derive(Clone, Debug)]
pub struct ProcessStopRequest {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub intent: ProcessStopIntent,
}

impl ProcessStopRequest {
    pub fn new(target: TargetDescriptor, intent: ProcessStopIntent) -> Self {
        Self {
            operation_id: None,
            target,
            intent,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessKillRequest {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub reason: ProcessKillReason,
    pub exit_code: Option<i32>,
}

impl ProcessKillRequest {
    pub fn new(target: TargetDescriptor, reason: ProcessKillReason) -> Self {
        Self {
            operation_id: None,
            target,
            reason,
            exit_code: None,
        }
    }

    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }
}

#[derive(Clone, Debug)]
pub struct ProcessObservationRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub observation: ProcessObservation<'a>,
}

impl<'a> ProcessObservationRequest<'a> {
    pub fn new(target: TargetDescriptor, observation: ProcessObservation<'a>) -> Self {
        Self {
            operation_id: None,
            target,
            observation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessKind {
    GameSession,
    Helper,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStopIntent {
    UserRequested,
    LauncherShutdown,
    GuardianRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessKillReason {
    UserRequested,
    StartupWatchdog,
    LauncherShutdown,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessObservation<'a> {
    Exited,
    ExitCode(i32),
    BootEvidence { label: &'a str },
    WatchdogAction(ProcessWatchdogAction),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessWatchdogAction {
    StartupNoOutputKill,
    StartupWindowExpired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCapabilityReport {
    pub target: TargetDescriptor,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Debug)]
pub struct ProcessCapabilityError {
    pub kind: ProcessCapabilityErrorKind,
    pub facts: Vec<ExecutionFact>,
}

impl ProcessCapabilityError {
    fn new(kind: ProcessCapabilityErrorKind, facts: Vec<ExecutionFact>) -> Self {
        Self { kind, facts }
    }
}

impl fmt::Display for ProcessCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ProcessCapabilityErrorKind::MissingPid => {
                formatter.write_str("process capability missing pid")
            }
        }
    }
}

impl std::error::Error for ProcessCapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessCapabilityErrorKind {
    MissingPid,
}

pub fn process_session_target(session_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Execution,
        TargetKind::Session,
        session_id,
        OwnershipClass::LauncherManaged,
    )
}

pub fn process_spawned(
    request: ProcessSpawnRequest<'_>,
    pid: u32,
) -> Result<ProcessCapabilityReport, ProcessCapabilityError> {
    let mut facts = Vec::new();
    if pid == 0 {
        facts.push(process_fact(
            ExecutionFactKind::PrimitiveRefused,
            request.operation_id,
            &request.target,
            Vec::new(),
        ));
        return Err(ProcessCapabilityError::new(
            ProcessCapabilityErrorKind::MissingPid,
            facts,
        ));
    }

    facts.push(process_fact(
        ExecutionFactKind::ProcessSpawned,
        request.operation_id,
        &request.target,
        vec![
            EvidenceField::new("pid", pid.to_string(), EvidenceSensitivity::Public),
            EvidenceField::new(
                "process_kind",
                process_kind_label(request.process_kind),
                EvidenceSensitivity::Public,
            ),
            EvidenceField::new(
                "command",
                sanitize_process_label(request.command_label, "session_process"),
                EvidenceSensitivity::Public,
            ),
        ],
    ));
    Ok(ProcessCapabilityReport {
        target: request.target,
        facts,
    })
}

pub fn process_stop_requested(request: ProcessStopRequest) -> ProcessCapabilityReport {
    let facts = vec![process_fact(
        ExecutionFactKind::ProcessStopIntent,
        request.operation_id,
        &request.target,
        vec![EvidenceField::new(
            "intent",
            stop_intent_label(request.intent),
            EvidenceSensitivity::Public,
        )],
    )];
    ProcessCapabilityReport {
        target: request.target,
        facts,
    }
}

pub fn process_killed(request: ProcessKillRequest) -> ProcessCapabilityReport {
    let mut facts = vec![process_fact(
        ExecutionFactKind::ProcessKilled,
        request.operation_id.clone(),
        &request.target,
        vec![EvidenceField::new(
            "reason",
            kill_reason_label(request.reason),
            EvidenceSensitivity::Public,
        )],
    )];
    if let Some(exit_code) = request.exit_code {
        facts.push(exit_code_fact(
            request.operation_id.clone(),
            &request.target,
            exit_code,
        ));
    }
    if request.reason == ProcessKillReason::StartupWatchdog {
        facts.push(process_fact(
            ExecutionFactKind::ProcessWatchdogAction,
            request.operation_id,
            &request.target,
            vec![EvidenceField::new(
                "action",
                watchdog_action_label(ProcessWatchdogAction::StartupNoOutputKill),
                EvidenceSensitivity::Public,
            )],
        ));
    }

    ProcessCapabilityReport {
        target: request.target,
        facts,
    }
}

pub fn observe_process(request: ProcessObservationRequest<'_>) -> ProcessCapabilityReport {
    let facts = match request.observation {
        ProcessObservation::Exited => vec![process_fact(
            ExecutionFactKind::ProcessExited,
            request.operation_id,
            &request.target,
            Vec::new(),
        )],
        ProcessObservation::ExitCode(exit_code) => vec![
            process_fact(
                ExecutionFactKind::ProcessExited,
                request.operation_id.clone(),
                &request.target,
                Vec::new(),
            ),
            exit_code_fact(request.operation_id, &request.target, exit_code),
        ],
        ProcessObservation::BootEvidence { label } => vec![process_fact(
            ExecutionFactKind::ProcessBootEvidence,
            request.operation_id,
            &request.target,
            vec![EvidenceField::new(
                "evidence",
                sanitize_process_label(Some(label), "boot_evidence"),
                EvidenceSensitivity::Public,
            )],
        )],
        ProcessObservation::WatchdogAction(action) => vec![process_fact(
            ExecutionFactKind::ProcessWatchdogAction,
            request.operation_id,
            &request.target,
            vec![EvidenceField::new(
                "action",
                watchdog_action_label(action),
                EvidenceSensitivity::Public,
            )],
        )],
    };

    ProcessCapabilityReport {
        target: request.target,
        facts,
    }
}

pub fn process_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    extra_fields: Vec<EvidenceField>,
) -> ExecutionFact {
    let mut fields = vec![EvidenceField::new(
        "target",
        target.id.clone(),
        EvidenceSensitivity::Public,
    )];
    fields.extend(extra_fields);
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields,
    }
}

pub fn process_stage_evidence(facts: &[ExecutionFact]) -> Vec<LaunchStageEvidence> {
    execution_fact_stage_evidence(facts)
}

pub fn process_spawn_failed_stage_evidence() -> LaunchStageEvidence {
    LaunchStageEvidence {
        id: "execution_process_spawn_failed".to_string(),
        system: "execution".to_string(),
        summary: sanitize_evidence_text(
            "Execution could not start the game process.",
            RedactionAudience::UserVisible,
            160,
        )
        .unwrap_or_else(|| "Execution recorded launch evidence.".to_string()),
        details: Vec::new(),
    }
}

fn exit_code_fact(
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    exit_code: i32,
) -> ExecutionFact {
    process_fact(
        ExecutionFactKind::ProcessExitCode,
        operation_id,
        target,
        vec![EvidenceField::new(
            "exit_code",
            exit_code.to_string(),
            EvidenceSensitivity::Public,
        )],
    )
}

fn sanitize_process_label(value: Option<&str>, fallback: &str) -> String {
    value
        .and_then(|value| sanitize_evidence_token(value, RedactionAudience::UserVisible, 64))
        .unwrap_or_else(|| fallback.to_string())
}

fn process_kind_label(kind: ProcessKind) -> &'static str {
    match kind {
        ProcessKind::GameSession => "game_session",
        ProcessKind::Helper => "helper",
    }
}

fn stop_intent_label(intent: ProcessStopIntent) -> &'static str {
    match intent {
        ProcessStopIntent::UserRequested => "user_requested",
        ProcessStopIntent::LauncherShutdown => "launcher_shutdown",
        ProcessStopIntent::GuardianRequested => "guardian_requested",
    }
}

fn kill_reason_label(reason: ProcessKillReason) -> &'static str {
    match reason {
        ProcessKillReason::UserRequested => "user_requested",
        ProcessKillReason::StartupWatchdog => "startup_watchdog",
        ProcessKillReason::LauncherShutdown => "launcher_shutdown",
        ProcessKillReason::Unknown => "unknown",
    }
}

fn watchdog_action_label(action: ProcessWatchdogAction) -> &'static str {
    match action {
        ProcessWatchdogAction::StartupNoOutputKill => "startup_no_output_kill",
        ProcessWatchdogAction::StartupWindowExpired => "startup_window_expired",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessCapabilityErrorKind, ProcessKillReason, ProcessKillRequest, ProcessObservation,
        ProcessObservationRequest, ProcessSpawnRequest, ProcessStopIntent, ProcessStopRequest,
        observe_process, process_killed, process_session_target, process_spawned,
        process_stop_requested,
    };
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn process_spawned_fact_redacts_raw_command_label() {
        let report = process_spawned(
            ProcessSpawnRequest::new(session_target(r"C:\Users\Alice\java.exe --accessToken abc"))
                .with_command_label("/home/alice/.jdks/java -Xmx8192M --accessToken secret"),
            4242,
        )
        .expect("spawned process report");

        assert!(has_fact(&report.facts, ExecutionFactKind::ProcessSpawned));
        let encoded = facts_json(&report.facts);
        assert!(encoded.contains("4242"));
        assert!(encoded.contains("session_process"));
        assert_no_sensitive_process_material(&encoded);
    }

    #[test]
    fn process_stage_evidence_is_redacted() {
        let report = process_spawned(
            ProcessSpawnRequest::new(session_target(r"C:\Users\Alice\java.exe --accessToken abc"))
                .with_command_label("/home/alice/.jdks/java -Xmx8192M --accessToken secret"),
            4242,
        )
        .expect("spawned process report");

        let evidence = super::process_stage_evidence(&report.facts);
        let encoded = serde_json::to_string(&evidence).expect("stage evidence json");

        assert!(encoded.contains("execution_process_spawned"));
        assert!(encoded.contains("pid:4242"));
        assert_no_sensitive_process_material(&encoded);
    }

    #[test]
    fn zero_pid_is_refused_with_bounded_error() {
        let error = process_spawned(ProcessSpawnRequest::new(session_target("session-1")), 0)
            .expect_err("zero pid should fail");

        assert_eq!(error.kind, ProcessCapabilityErrorKind::MissingPid);
        assert!(has_fact(&error.facts, ExecutionFactKind::PrimitiveRefused));
        assert_no_sensitive_process_material(&facts_json(&error.facts));
    }

    #[test]
    fn stop_intent_and_forced_kill_are_distinct_facts() {
        let stop = process_stop_requested(ProcessStopRequest::new(
            session_target("session-1"),
            ProcessStopIntent::UserRequested,
        ));
        let kill = process_killed(
            ProcessKillRequest::new(
                session_target("session-1"),
                ProcessKillReason::UserRequested,
            )
            .with_exit_code(-9),
        );

        assert!(has_fact(&stop.facts, ExecutionFactKind::ProcessStopIntent));
        assert!(!has_fact(&stop.facts, ExecutionFactKind::ProcessKilled));
        assert!(has_fact(&kill.facts, ExecutionFactKind::ProcessKilled));
        assert!(has_fact(&kill.facts, ExecutionFactKind::ProcessExitCode));
        assert!(!has_fact(&kill.facts, ExecutionFactKind::ProcessStopIntent));
    }

    #[test]
    fn watchdog_kill_emits_kill_and_watchdog_facts() {
        let report = process_killed(ProcessKillRequest::new(
            session_target("session-1"),
            ProcessKillReason::StartupWatchdog,
        ));

        assert!(has_fact(&report.facts, ExecutionFactKind::ProcessKilled));
        assert!(has_fact(
            &report.facts,
            ExecutionFactKind::ProcessWatchdogAction
        ));
        let encoded = facts_json(&report.facts);
        assert!(encoded.contains("startup_watchdog"));
        assert!(encoded.contains("startup_no_output_kill"));
    }

    #[test]
    fn exit_code_observation_does_not_classify_crash_or_clean_exit() {
        let report = observe_process(ProcessObservationRequest::new(
            session_target("session-1"),
            ProcessObservation::ExitCode(0),
        ));

        assert!(has_fact(&report.facts, ExecutionFactKind::ProcessExited));
        assert!(has_fact(&report.facts, ExecutionFactKind::ProcessExitCode));
        let encoded = facts_json(&report.facts);
        assert!(encoded.contains("exit_code"));
        assert!(!encoded.contains("crash"));
        assert!(!encoded.contains("clean"));
    }

    #[test]
    fn process_exit_observation_can_omit_exit_code() {
        let report = observe_process(ProcessObservationRequest::new(
            session_target("session-1"),
            ProcessObservation::Exited,
        ));

        assert!(has_fact(&report.facts, ExecutionFactKind::ProcessExited));
        assert!(!has_fact(&report.facts, ExecutionFactKind::ProcessExitCode));
        assert_no_sensitive_process_material(&facts_json(&report.facts));
    }

    #[test]
    fn process_session_target_is_execution_owned_launcher_managed_session() {
        let target = process_session_target("session-1");

        assert_eq!(target.system, StabilizationSystem::Execution);
        assert_eq!(target.kind, TargetKind::Session);
        assert_eq!(target.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(target.id, "session-1");
    }

    #[test]
    fn boot_evidence_observation_redacts_log_like_payload() {
        let report = observe_process(ProcessObservationRequest::new(
            session_target("session-1"),
            ProcessObservation::BootEvidence {
                label: "Started with /home/alice/.minecraft --classpath secret -Xmx8192M",
            },
        ));

        assert!(has_fact(
            &report.facts,
            ExecutionFactKind::ProcessBootEvidence
        ));
        let encoded = facts_json(&report.facts);
        assert!(encoded.contains("boot_evidence"));
        assert_no_sensitive_process_material(&encoded);
    }

    fn session_target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Session,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    fn has_fact(facts: &[crate::execution::ExecutionFact], kind: ExecutionFactKind) -> bool {
        facts.iter().any(|fact| fact.kind == kind)
    }

    fn facts_json(facts: &[crate::execution::ExecutionFact]) -> String {
        serde_json::to_string(facts).expect("facts json")
    }

    fn assert_no_sensitive_process_material(encoded: &str) {
        let lower = encoded.to_ascii_lowercase();
        assert!(!lower.contains("/home/"));
        assert!(!lower.contains("users\\\\alice"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--classpath"));
        assert!(!lower.contains("--accesstoken"));
        assert!(!lower.contains("secret"));
    }
}
