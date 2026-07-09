use super::{FactReliability, GuardianDomain, GuardianFact, GuardianFactId, GuardianObservation};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, RedactionAudience, sanitize_evidence_token};
use crate::state::contracts::{OperationPhase, OwnershipClass, TargetDescriptor};

pub fn guardian_fact_from_execution(fact: &ExecutionFact, phase: OperationPhase) -> GuardianFact {
    let (id, domain, reliability) = execution_fact_shape(fact);
    let target = fact.target.as_ref().map(public_safe_target);
    let ownership = target
        .as_ref()
        .map(|target| target.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    GuardianFact {
        operation_id: fact.operation_id.clone(),
        id,
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target,
        fields: public_safe_fields(&fact.fields),
    }
}

pub fn guardian_fact_from_observation(
    observation: GuardianObservation,
    phase: OperationPhase,
    target: Option<TargetDescriptor>,
) -> GuardianFact {
    let (id, domain, reliability) = observation_fact_shape(&observation);
    let target = target.as_ref().map(public_safe_target);
    let ownership = target
        .as_ref()
        .map(|target| target.ownership)
        .unwrap_or(OwnershipClass::Unknown);
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target,
        fields: Vec::new(),
    }
}

fn execution_fact_shape(fact: &ExecutionFact) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let id = match fact.kind {
        ExecutionFactKind::ArtifactMissing | ExecutionFactKind::FileMissing => "artifact_missing",
        ExecutionFactKind::ArtifactVerified => "artifact_verified",
        ExecutionFactKind::ChecksumMismatch | ExecutionFactKind::DownloadChecksumMismatch => {
            "artifact_checksum_mismatch"
        }
        ExecutionFactKind::SizeMismatch | ExecutionFactKind::DownloadSizeMismatch => {
            "artifact_size_mismatch"
        }
        ExecutionFactKind::DownloadProviderFailure => "download_provider_unavailable",
        ExecutionFactKind::DownloadNetworkFailure | ExecutionFactKind::DownloadInterrupted => {
            "download_interrupted"
        }
        ExecutionFactKind::DownloadTempDiscarded => "download_temp_discarded",
        ExecutionFactKind::DownloadTempWriteFailed => "temp_file_leftover",
        ExecutionFactKind::DownloadWrittenToTemp => "download_written_to_temp",
        ExecutionFactKind::DownloadPromotionFailed => "atomic_promotion_failed",
        ExecutionFactKind::DownloadPromoted | ExecutionFactKind::FilePromoted => {
            "atomic_promotion_completed"
        }
        ExecutionFactKind::FileCorrupt => "managed_file_corrupt",
        ExecutionFactKind::FileLocked => "filesystem_locked",
        ExecutionFactKind::FileOwnershipUnknown => "ownership_unknown",
        ExecutionFactKind::FilePermissionDenied => "filesystem_permission_denied",
        ExecutionFactKind::FileQuarantined => "artifact_quarantined",
        ExecutionFactKind::FileTempLeftover => "temp_file_leftover",
        ExecutionFactKind::FileWrittenToTemp => "file_written_to_temp",
        ExecutionFactKind::InstallDependencyFailed => "install_dependency_failed",
        ExecutionFactKind::RuntimeCorrupt => "managed_runtime_corrupt",
        ExecutionFactKind::RuntimeJavaOverrideEmpty => "java_override_empty",
        ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => {
            "java_override_undefined_sentinel"
        }
        ExecutionFactKind::RuntimeMissingExecutable => {
            if fact
                .target
                .as_ref()
                .is_some_and(|target| target.ownership == OwnershipClass::UserOwned)
            {
                "java_override_missing"
            } else {
                "managed_runtime_missing"
            }
        }
        ExecutionFactKind::RuntimeProbeFailed => "java_probe_failed",
        ExecutionFactKind::RuntimeReadyMarkerMissing => "managed_runtime_ready_marker_missing",
        ExecutionFactKind::RuntimeRepairApplied => "managed_runtime_repair_applied",
        ExecutionFactKind::RuntimeRosettaRequired => "managed_runtime_rosetta_required",
        ExecutionFactKind::RuntimeUnavailableForPlatform => {
            "managed_runtime_unavailable_for_platform"
        }
        ExecutionFactKind::RuntimeWrongMajor => "java_major_mismatch",
        ExecutionFactKind::RuntimeWrongUpdate => "java_update_too_old",
        ExecutionFactKind::JvmArgsEmpty => "jvm_args_empty",
        ExecutionFactKind::JvmArgsParseFailed => "jvm_args_parse_failed",
        ExecutionFactKind::JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
        ExecutionFactKind::JvmArgMemoryConflict => "jvm_arg_memory_conflict",
        ExecutionFactKind::JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
        ExecutionFactKind::JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
        ExecutionFactKind::JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
        ExecutionFactKind::JvmArgUnsafeNativePathOverride => "jvm_arg_unsafe_native_path_override",
        ExecutionFactKind::JvmArgAgentOverride => "jvm_arg_agent_override",
        ExecutionFactKind::LaunchCommandInvalid => "launch_command_invalid",
        ExecutionFactKind::LaunchCommandPrepared => "launch_command_prepared",
        ExecutionFactKind::ProcessSpawned => "process_spawned",
        ExecutionFactKind::ProcessStopIntent => "launcher_stop_requested",
        ExecutionFactKind::ProcessKilled => "watchdog_killed_process",
        ExecutionFactKind::ProcessExitCode => exit_code_fact_id(fact),
        ExecutionFactKind::ProcessBootEvidence => "boot_marker_observed",
        ExecutionFactKind::ProcessWatchdogAction => "watchdog_killed_process",
        ExecutionFactKind::ProcessExited => "process_exited",
        ExecutionFactKind::PrimitiveRefused => "primitive_refused",
        ExecutionFactKind::ProviderDataInvalid => "provider_data_invalid",
        ExecutionFactKind::RollbackAvailable => "rollback_available",
        ExecutionFactKind::RollbackUnavailable => "rollback_unavailable",
    };
    (
        GuardianFactId::new(id),
        domain_for_fact_id(id),
        reliability_for_execution_fact(fact.kind),
    )
}

fn observation_fact_shape(
    observation: &GuardianObservation,
) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let id = match observation {
        GuardianObservation::JavaOverrideEmpty => "java_override_empty",
        GuardianObservation::JavaOverrideUndefinedSentinel => "java_override_undefined_sentinel",
        GuardianObservation::JavaOverrideMissing => "java_override_missing",
        GuardianObservation::JavaProbeFailed => "java_probe_failed",
        GuardianObservation::JavaMajorMismatch => "java_major_mismatch",
        GuardianObservation::JvmArgsParseFailed => "jvm_args_parse_failed",
        GuardianObservation::JvmArgReservedLauncherFlag => "jvm_arg_reserved_launcher_flag",
        GuardianObservation::JvmArgMemoryConflict => "jvm_arg_memory_conflict",
        GuardianObservation::JvmArgUnsupportedGc => "jvm_arg_unsupported_gc",
        GuardianObservation::JvmArgUnlockOrderInvalid => "jvm_arg_unlock_order_invalid",
        GuardianObservation::JvmArgUnsafeClasspathOverride => "jvm_arg_unsafe_classpath_override",
        GuardianObservation::JvmArgUnsafeNativePathOverride => {
            "jvm_arg_unsafe_native_path_override"
        }
        GuardianObservation::JvmArgAgentOverride => "jvm_arg_agent_override",
        GuardianObservation::RawJvmArgsPresent => "raw_jvm_args_present",
        GuardianObservation::ProcessExitedBeforeBoot => "process_exited_before_boot",
        GuardianObservation::ProcessExitedAfterBoot => "process_exited_after_boot",
        GuardianObservation::BootMarkerObserved => "boot_marker_observed",
        GuardianObservation::LauncherStopRequested => "launcher_stop_requested",
        GuardianObservation::PersistedStateSchemaInvalid => "persisted_state_schema_invalid",
        GuardianObservation::Unknown(value) => value.as_str(),
    };
    (
        GuardianFactId::new(sanitize_fact_id(id)),
        domain_for_fact_id(id),
        reliability_for_observation(observation),
    )
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        target.id.as_str(),
        target.ownership,
    )
}

fn public_safe_fields(fields: &[EvidenceField]) -> Vec<EvidenceField> {
    fields
        .iter()
        .filter_map(|field| {
            field
                .value_for(RedactionAudience::UserVisible)
                .and_then(|value| {
                    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
                })
                .map(|value| EvidenceField::new(field.key.clone(), value, field.sensitivity))
        })
        .collect()
}

fn exit_code_fact_id(fact: &ExecutionFact) -> &'static str {
    let exit_code = fact
        .fields
        .iter()
        .find(|field| field.key == "exit_code")
        .and_then(|field| field.value.parse::<i32>().ok());
    match exit_code {
        Some(0) => "exit_code_zero",
        Some(_) => "exit_code_nonzero",
        None => "exit_code_unknown",
    }
}

fn reliability_for_execution_fact(kind: ExecutionFactKind) -> FactReliability {
    match kind {
        ExecutionFactKind::RuntimeProbeFailed
        | ExecutionFactKind::RuntimeRosettaRequired
        | ExecutionFactKind::RuntimeUnavailableForPlatform
        | ExecutionFactKind::RuntimeWrongMajor
        | ExecutionFactKind::RuntimeWrongUpdate
        | ExecutionFactKind::DownloadChecksumMismatch
        | ExecutionFactKind::DownloadSizeMismatch
        | ExecutionFactKind::ChecksumMismatch
        | ExecutionFactKind::SizeMismatch => FactReliability::ValidatedProbe,
        ExecutionFactKind::RuntimeJavaOverrideEmpty
        | ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => {
            FactReliability::ExactClassifier
        }
        ExecutionFactKind::JvmArgsParseFailed
        | ExecutionFactKind::JvmArgReservedLauncherFlag
        | ExecutionFactKind::JvmArgMemoryConflict
        | ExecutionFactKind::JvmArgUnsupportedGc
        | ExecutionFactKind::JvmArgUnlockOrderInvalid
        | ExecutionFactKind::JvmArgUnsafeClasspathOverride
        | ExecutionFactKind::JvmArgUnsafeNativePathOverride
        | ExecutionFactKind::JvmArgAgentOverride => FactReliability::ExactClassifier,
        ExecutionFactKind::ProcessSpawned
        | ExecutionFactKind::ProcessStopIntent
        | ExecutionFactKind::ProcessKilled
        | ExecutionFactKind::ProcessExitCode
        | ExecutionFactKind::ProcessBootEvidence
        | ExecutionFactKind::ProcessWatchdogAction
        | ExecutionFactKind::ProcessExited => FactReliability::ProcessLifecycle,
        ExecutionFactKind::RuntimeReadyMarkerMissing => FactReliability::ExpectedMarkerAbsence,
        _ => FactReliability::DirectStructured,
    }
}

fn reliability_for_observation(observation: &GuardianObservation) -> FactReliability {
    match observation {
        GuardianObservation::BootMarkerObserved
        | GuardianObservation::LauncherStopRequested
        | GuardianObservation::ProcessExitedBeforeBoot
        | GuardianObservation::ProcessExitedAfterBoot => FactReliability::ProcessLifecycle,
        GuardianObservation::JavaProbeFailed | GuardianObservation::JavaMajorMismatch => {
            FactReliability::ValidatedProbe
        }
        GuardianObservation::JvmArgsParseFailed
        | GuardianObservation::JvmArgReservedLauncherFlag
        | GuardianObservation::JvmArgMemoryConflict
        | GuardianObservation::JvmArgUnsupportedGc
        | GuardianObservation::JvmArgUnlockOrderInvalid
        | GuardianObservation::JvmArgUnsafeClasspathOverride
        | GuardianObservation::JvmArgUnsafeNativePathOverride
        | GuardianObservation::JvmArgAgentOverride => FactReliability::ExactClassifier,
        GuardianObservation::RawJvmArgsPresent | GuardianObservation::Unknown(_) => {
            FactReliability::HeuristicClassifier
        }
        GuardianObservation::PersistedStateSchemaInvalid => FactReliability::DirectStructured,
        _ => FactReliability::DirectStructured,
    }
}

fn domain_for_fact_id(id: &str) -> GuardianDomain {
    if id.starts_with("java_") || id.starts_with("managed_runtime") {
        GuardianDomain::Runtime
    } else if id.starts_with("jvm_") || id == "raw_jvm_args_present" {
        GuardianDomain::Jvm
    } else if id.starts_with("launch_command") {
        GuardianDomain::Launch
    } else if matches!(
        id,
        "version_json_missing"
            | "parent_version_missing"
            | "incomplete_install"
            | "client_jar_missing"
            | "libraries_missing"
            | "asset_index_missing"
            | "install_dependency_failed"
    ) {
        GuardianDomain::Install
    } else if id.starts_with("download_") {
        GuardianDomain::Download
    } else if id.starts_with("process_")
        || id.starts_with("exit_code")
        || id == "boot_marker_observed"
        || id == "launcher_stop_requested"
        || id == "watchdog_killed_process"
    {
        GuardianDomain::Session
    } else if id.contains("artifact") || id.starts_with("file_") {
        GuardianDomain::Library
    } else if id.starts_with("filesystem_")
        || id == "temp_file_leftover"
        || id.starts_with("atomic_promotion_")
    {
        GuardianDomain::Filesystem
    } else if id.starts_with("provider") {
        GuardianDomain::Network
    } else if id.starts_with("performance_") {
        GuardianDomain::Performance
    } else if id.starts_with("persisted_state_") || id.starts_with("state_") {
        GuardianDomain::State
    } else {
        GuardianDomain::Unknown
    }
}

fn sanitize_fact_id(id: &str) -> String {
    sanitize_evidence_token(id, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "unknown_fact".to_string())
}
