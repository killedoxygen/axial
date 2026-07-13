//! Execution system boundary.
//!
//! Execution owns bounded concrete capabilities and reports facts about low
//! level work. These contracts do not authorize product policy decisions.

pub(crate) mod crash;
pub mod download;
pub mod file;
pub(crate) mod integrity;
pub mod jvm;
pub mod launch;
mod low_priority;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExecutionFactSemantics {
    Diagnostic,
    ConditionEvidence,
    NonFailure,
}

impl ExecutionFactSemantics {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostic => "diagnostic",
            Self::ConditionEvidence => "condition_evidence",
            Self::NonFailure => "non_failure",
        }
    }
}

macro_rules! execution_fact_kinds {
    ($($variant:ident => ($id:literal, $semantics:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum ExecutionFactKind {
            $($variant),+
        }

        impl ExecutionFactKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $id),+
                }
            }

            pub const fn semantics(self) -> ExecutionFactSemantics {
                match self {
                    $(Self::$variant => ExecutionFactSemantics::$semantics),+
                }
            }
        }
    };
}

execution_fact_kinds! {
    ArtifactHashMismatch => ("artifact_hash_mismatch", Diagnostic),
    ArtifactMissing => ("artifact_missing", Diagnostic),
    ArtifactSizeDrift => ("artifact_size_drift", Diagnostic),
    DownloadChecksumMismatch => ("download_checksum_mismatch", Diagnostic),
    DownloadInterrupted => ("download_interrupted", Diagnostic),
    DownloadNetworkFailure => ("download_network_failure", Diagnostic),
    DownloadPromoted => ("download_promoted", NonFailure),
    DownloadPromotionFailed => ("download_promotion_failed", Diagnostic),
    DownloadProviderFailure => ("download_provider_failure", Diagnostic),
    DownloadSizeMismatch => ("download_size_mismatch", Diagnostic),
    DownloadTempDiscarded => ("download_temp_discarded", NonFailure),
    DownloadTempWriteFailed => ("download_temp_write_failed", Diagnostic),
    DownloadWrittenToTemp => ("download_written_to_temp", NonFailure),
    FileLocked => ("file_locked", Diagnostic),
    FileMissing => ("file_missing", Diagnostic),
    FileOwnershipUnknown => ("file_ownership_unknown", Diagnostic),
    FilePermissionDenied => ("file_permission_denied", Diagnostic),
    FileQuarantined => ("file_quarantined", NonFailure),
    FilePromoted => ("file_promoted", NonFailure),
    FileTempLeftover => ("file_temp_leftover", ConditionEvidence),
    FileWrittenToTemp => ("file_written_to_temp", NonFailure),
    InstallDependencyFailed => ("install_dependency_failed", Diagnostic),
    InstallExecutionFailed => ("install_execution_failed", Diagnostic),
    InstallProcessorFailed => ("install_processor_failed", Diagnostic),
    RuntimeCorrupt => ("runtime_corrupt", Diagnostic),
    RuntimeJavaOverrideEmpty => ("runtime_java_override_empty", ConditionEvidence),
    RuntimeJavaOverrideUndefinedSentinel => ("runtime_java_override_undefined_sentinel", ConditionEvidence),
    RuntimeMissingExecutable => ("runtime_missing_executable", Diagnostic),
    RuntimeProbeFailed => ("runtime_probe_failed", Diagnostic),
    RuntimeReadyMarkerMissing => ("runtime_ready_marker_missing", ConditionEvidence),
    RuntimeRepairApplied => ("runtime_repair_applied", NonFailure),
    RuntimeRosettaRequired => ("runtime_rosetta_required", Diagnostic),
    RuntimeUnavailableForPlatform => ("runtime_unavailable_for_platform", Diagnostic),
    RuntimeWrongMajor => ("runtime_wrong_major", ConditionEvidence),
    RuntimeWrongUpdate => ("runtime_wrong_update", ConditionEvidence),
    JvmArgsEmpty => ("jvm_args_empty", ConditionEvidence),
    JvmArgsParseFailed => ("jvm_args_parse_failed", ConditionEvidence),
    JvmArgReservedLauncherFlag => ("jvm_arg_reserved_launcher_flag", ConditionEvidence),
    JvmArgMemoryConflict => ("jvm_arg_memory_conflict", ConditionEvidence),
    JvmArgUnsupportedGc => ("jvm_arg_unsupported_gc", ConditionEvidence),
    JvmArgUnlockOrderInvalid => ("jvm_arg_unlock_order_invalid", ConditionEvidence),
    JvmArgUnsafeClasspathOverride => ("jvm_arg_unsafe_classpath_override", ConditionEvidence),
    JvmArgUnsafeNativePathOverride => ("jvm_arg_unsafe_native_path_override", ConditionEvidence),
    JvmArgAgentOverride => ("jvm_arg_agent_override", ConditionEvidence),
    LaunchCommandInvalid => ("launch_command_invalid", Diagnostic),
    LaunchCommandPrepared => ("launch_command_prepared", ConditionEvidence),
    ProcessSpawned => ("process_spawned", ConditionEvidence),
    ProcessStopIntent => ("process_stop_intent", ConditionEvidence),
    ProcessKilled => ("process_killed", ConditionEvidence),
    ProcessExitCode => ("process_exit_code", ConditionEvidence),
    ProcessBootEvidence => ("process_boot_evidence", ConditionEvidence),
    ProcessWatchdogAction => ("process_watchdog_action", ConditionEvidence),
    ProcessExited => ("process_exited", ConditionEvidence),
    PrimitiveRefused => ("primitive_refused", Diagnostic),
    ProviderDataInvalid => ("provider_data_invalid", Diagnostic),
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
