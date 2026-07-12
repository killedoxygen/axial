//! Execution system boundary.
//!
//! Execution owns bounded concrete capabilities and reports facts about low
//! level work. These contracts do not authorize product policy decisions.

pub(crate) mod crash;
pub mod download;
pub mod file;
pub mod jvm;
pub mod launch;
pub(crate) mod persistence;
pub mod process;
pub mod runtime;

use crate::observability::{
    EvidenceField, RedactionAudience, sanitize_evidence_text, sanitize_evidence_token,
};
use crate::state::contracts::{OperationId, OwnershipClass, TargetDescriptor};
use axial_launcher::LaunchStageEvidence;
use serde::{Deserialize, Serialize};

const MAX_STAGE_EVIDENCE_DETAILS: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityContract {
    pub kind: ExecutionCapabilityKind,
    pub target: TargetDescriptor,
    pub required_ownership: OwnershipClass,
    pub rollback: RollbackBehavior,
    pub sensitive_fields: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionCapabilityKind {
    VerifyArtifact,
    DownloadArtifactToTemp,
    PromoteVerifiedArtifact,
    QuarantineLauncherManagedPath,
    RepairManagedRuntime,
    VerifyManagedRuntime,
    ProbeJavaRuntime,
    PrepareLaunchCommand,
    SpawnSessionProcess,
    StopSessionProcess,
    KillSessionProcess,
    ObserveSessionProcess,
    RestoreRollbackSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RollbackBehavior {
    None,
    JournalOnly,
    SnapshotRequired,
    RestoresSnapshot,
    QuarantineOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionFact {
    pub operation_id: Option<OperationId>,
    pub kind: ExecutionFactKind,
    pub target: Option<TargetDescriptor>,
    pub fields: Vec<EvidenceField>,
}

macro_rules! execution_fact_kinds {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum ExecutionFactKind {
            $($variant),+
        }

        impl ExecutionFactKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

execution_fact_kinds! {
    ArtifactMissing,
    ArtifactVerified,
    ChecksumMismatch,
    DownloadChecksumMismatch,
    DownloadInterrupted,
    DownloadNetworkFailure,
    DownloadPromoted,
    DownloadPromotionFailed,
    DownloadProviderFailure,
    DownloadSizeMismatch,
    DownloadTempDiscarded,
    DownloadTempWriteFailed,
    DownloadWrittenToTemp,
    SizeMismatch,
    FileCorrupt,
    FileLocked,
    FileMissing,
    FileOwnershipUnknown,
    FilePermissionDenied,
    FileQuarantined,
    FilePromoted,
    FileTempLeftover,
    FileWrittenToTemp,
    InstallDependencyFailed,
    InstallExecutionFailed,
    InstallProcessorFailed,
    RuntimeCorrupt,
    RuntimeJavaOverrideEmpty,
    RuntimeJavaOverrideUndefinedSentinel,
    RuntimeMissingExecutable,
    RuntimeProbeFailed,
    RuntimeReadyMarkerMissing,
    RuntimeRepairApplied,
    RuntimeRosettaRequired,
    RuntimeUnavailableForPlatform,
    RuntimeWrongMajor,
    RuntimeWrongUpdate,
    JvmArgsEmpty,
    JvmArgsParseFailed,
    JvmArgReservedLauncherFlag,
    JvmArgMemoryConflict,
    JvmArgUnsupportedGc,
    JvmArgUnlockOrderInvalid,
    JvmArgUnsafeClasspathOverride,
    JvmArgUnsafeNativePathOverride,
    JvmArgAgentOverride,
    LaunchCommandInvalid,
    LaunchCommandPrepared,
    ProcessSpawned,
    ProcessStopIntent,
    ProcessKilled,
    ProcessExitCode,
    ProcessBootEvidence,
    ProcessWatchdogAction,
    ProcessExited,
    PrimitiveRefused,
    ProviderDataInvalid,
    RollbackAvailable,
    RollbackUnavailable,
}

pub fn execution_fact_stage_evidence(facts: &[ExecutionFact]) -> Vec<LaunchStageEvidence> {
    facts
        .iter()
        .map(|fact| {
            let (id, summary) = execution_fact_stage_copy(fact.kind);
            LaunchStageEvidence {
                id: sanitize_evidence_token(id, RedactionAudience::UserVisible, 64)
                    .unwrap_or_else(|| "execution_fact".to_string()),
                system: "execution".to_string(),
                summary: sanitize_evidence_text(summary, RedactionAudience::UserVisible, 160)
                    .unwrap_or_else(|| "Execution recorded launch evidence.".to_string()),
                details: execution_fact_stage_details(&fact.fields),
            }
        })
        .collect()
}

fn execution_fact_stage_copy(kind: ExecutionFactKind) -> (&'static str, &'static str) {
    match kind {
        ExecutionFactKind::LaunchCommandPrepared => (
            "execution_launch_command_prepared",
            "Execution prepared a runnable launch command.",
        ),
        ExecutionFactKind::LaunchCommandInvalid => (
            "execution_launch_command_invalid",
            "Execution rejected a non-runnable launch command.",
        ),
        ExecutionFactKind::ProcessSpawned => (
            "execution_process_spawned",
            "Execution started the game process.",
        ),
        ExecutionFactKind::ProcessStopIntent => (
            "execution_process_stop_requested",
            "Execution recorded a process stop request.",
        ),
        ExecutionFactKind::ProcessKilled => (
            "execution_process_killed",
            "Execution killed the game process.",
        ),
        ExecutionFactKind::ProcessExitCode => (
            "execution_process_exit_code",
            "Execution recorded the process exit code.",
        ),
        ExecutionFactKind::ProcessBootEvidence => (
            "execution_process_boot_evidence",
            "Execution observed Minecraft startup evidence.",
        ),
        ExecutionFactKind::ProcessWatchdogAction => (
            "execution_process_watchdog_action",
            "Execution recorded a process watchdog action.",
        ),
        ExecutionFactKind::ProcessExited => (
            "execution_process_exited",
            "Execution observed the game process exit.",
        ),
        ExecutionFactKind::PrimitiveRefused => (
            "execution_primitive_refused",
            "Execution refused an impossible primitive action.",
        ),
        _ => (
            "execution_fact_recorded",
            "Execution recorded launch evidence.",
        ),
    }
}

fn execution_fact_stage_details(fields: &[EvidenceField]) -> Vec<String> {
    fields
        .iter()
        .filter_map(|field| {
            let key = sanitize_evidence_token(&field.key, RedactionAudience::UserVisible, 32)?;
            let value = field.value_for(RedactionAudience::UserVisible)?;
            let value = sanitize_evidence_token(value, RedactionAudience::UserVisible, 64)?;
            sanitize_evidence_text(
                &format!("{key}:{value}"),
                RedactionAudience::UserVisible,
                120,
            )
        })
        .take(MAX_STAGE_EVIDENCE_DETAILS)
        .collect()
}
