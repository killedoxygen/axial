//! Execution-owned launch command preparation capability.
//!
//! This module validates the primitive command shape and reports facts. It does
//! not choose Java, rewrite arguments, spawn processes, or decide Guardian policy.

use super::{ExecutionFact, ExecutionFactKind, execution_fact_stage_evidence};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{OperationId, TargetDescriptor};
use croopor_launcher::LaunchStageEvidence;
use std::fmt;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct LaunchCommandPreparationRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub command: &'a [String],
    pub game_dir: &'a Path,
}

impl<'a> LaunchCommandPreparationRequest<'a> {
    pub fn new(target: TargetDescriptor, command: &'a [String], game_dir: &'a Path) -> Self {
        Self {
            operation_id: None,
            target,
            command,
            game_dir,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedLaunchCommand<'a> {
    pub target: TargetDescriptor,
    pub program: &'a str,
    pub args: &'a [String],
    pub game_dir: &'a Path,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Debug)]
pub struct LaunchCommandPreparationError {
    pub kind: LaunchCommandPreparationErrorKind,
    pub facts: Vec<ExecutionFact>,
}

impl LaunchCommandPreparationError {
    fn new(kind: LaunchCommandPreparationErrorKind, facts: Vec<ExecutionFact>) -> Self {
        Self { kind, facts }
    }
}

impl fmt::Display for LaunchCommandPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            LaunchCommandPreparationErrorKind::NotRunnable => {
                formatter.write_str("launch command did not produce a runnable command")
            }
        }
    }
}

impl std::error::Error for LaunchCommandPreparationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchCommandPreparationErrorKind {
    NotRunnable,
}

pub fn prepare_launch_command<'a>(
    request: LaunchCommandPreparationRequest<'a>,
) -> Result<PreparedLaunchCommand<'a>, LaunchCommandPreparationError> {
    if request.command.len() < 2 {
        let facts = vec![launch_command_fact(
            ExecutionFactKind::LaunchCommandInvalid,
            request.operation_id,
            &request.target,
            vec![EvidenceField::new(
                "arg_count",
                request.command.len().to_string(),
                EvidenceSensitivity::Public,
            )],
        )];
        return Err(LaunchCommandPreparationError::new(
            LaunchCommandPreparationErrorKind::NotRunnable,
            facts,
        ));
    }

    let facts = vec![launch_command_fact(
        ExecutionFactKind::LaunchCommandPrepared,
        request.operation_id,
        &request.target,
        vec![
            EvidenceField::new(
                "arg_count",
                request.command.len().to_string(),
                EvidenceSensitivity::Public,
            ),
            EvidenceField::new(
                "program",
                sanitize_command_program(&request.command[0]),
                EvidenceSensitivity::Public,
            ),
        ],
    )];

    Ok(PreparedLaunchCommand {
        target: request.target,
        program: request.command[0].as_str(),
        args: &request.command[1..],
        game_dir: request.game_dir,
        facts,
    })
}

pub fn launch_command_fact(
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

pub fn launch_command_stage_evidence(facts: &[ExecutionFact]) -> Vec<LaunchStageEvidence> {
    execution_fact_stage_evidence(facts)
}

fn sanitize_command_program(value: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)
        .unwrap_or_else(|| "launch_program".to_string())
}

#[cfg(test)]
mod tests {
    use super::{LaunchCommandPreparationRequest, prepare_launch_command};
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use std::path::Path;

    #[test]
    fn prepare_launch_command_reports_prepared_fact_without_raw_path() {
        let command = vec![
            r"C:\Users\Alice\.jdks\java.exe".to_string(),
            "-cp".to_string(),
            "libraries".to_string(),
        ];

        let prepared = prepare_launch_command(LaunchCommandPreparationRequest::new(
            launch_target("session-1"),
            &command,
            Path::new("/home/alice/.minecraft"),
        ))
        .expect("prepared command");

        assert_eq!(prepared.program, command[0]);
        assert_eq!(prepared.args, &command[1..]);
        assert_eq!(
            prepared.facts[0].kind,
            ExecutionFactKind::LaunchCommandPrepared
        );
        assert_no_sensitive_command_material(&prepared.facts);
    }

    #[test]
    fn prepare_launch_command_rejects_short_command_without_raw_path() {
        let command = vec![r"C:\Users\Alice\.jdks\java.exe".to_string()];

        let error = prepare_launch_command(LaunchCommandPreparationRequest::new(
            launch_target("session-1"),
            &command,
            Path::new("/home/alice/.minecraft"),
        ))
        .expect_err("short command should fail");

        assert_eq!(error.facts[0].kind, ExecutionFactKind::LaunchCommandInvalid);
        assert_no_sensitive_command_material(&error.facts);
    }

    #[test]
    fn launch_command_stage_evidence_is_redacted() {
        let command = vec![
            r"C:\Users\Alice\.jdks\java.exe".to_string(),
            "-cp".to_string(),
            "libraries".to_string(),
        ];
        let prepared = prepare_launch_command(LaunchCommandPreparationRequest::new(
            launch_target("session-1"),
            &command,
            Path::new("/home/alice/.minecraft"),
        ))
        .expect("prepared command");

        let evidence = super::launch_command_stage_evidence(&prepared.facts);
        let encoded = serde_json::to_string(&evidence).expect("stage evidence json");

        assert!(encoded.contains("execution_launch_command_prepared"));
        assert!(encoded.contains("arg_count:3"));
        assert_no_sensitive_command_material(&prepared.facts);
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("java.exe"));
        assert!(!encoded.contains("-cp"));
        assert!(!encoded.contains("/home/"));
    }

    fn launch_target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Session,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    fn assert_no_sensitive_command_material(facts: &[crate::execution::ExecutionFact]) {
        let encoded = serde_json::to_string(facts).expect("facts json");
        let lower = encoded.to_ascii_lowercase();
        assert!(!lower.contains("alice"));
        assert!(!lower.contains(".jdks"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("/home/"));
        assert!(!lower.contains("-cp"));
    }
}
