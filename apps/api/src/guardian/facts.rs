use super::{FactReliability, GuardianDomain, GuardianFact, GuardianFactId};
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

fn execution_fact_shape(fact: &ExecutionFact) -> (GuardianFactId, GuardianDomain, FactReliability) {
    let (id, domain) = match fact.kind {
        ExecutionFactKind::ArtifactMissing | ExecutionFactKind::FileMissing => {
            (GuardianFactId::ArtifactMissing, GuardianDomain::Library)
        }
        ExecutionFactKind::ArtifactVerified => {
            (GuardianFactId::ArtifactVerified, GuardianDomain::Library)
        }
        ExecutionFactKind::ChecksumMismatch | ExecutionFactKind::DownloadChecksumMismatch => (
            GuardianFactId::ArtifactChecksumMismatch,
            GuardianDomain::Library,
        ),
        ExecutionFactKind::SizeMismatch | ExecutionFactKind::DownloadSizeMismatch => (
            GuardianFactId::ArtifactSizeMismatch,
            GuardianDomain::Library,
        ),
        ExecutionFactKind::DownloadProviderFailure => (
            GuardianFactId::DownloadProviderUnavailable,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadNetworkFailure | ExecutionFactKind::DownloadInterrupted => (
            GuardianFactId::DownloadInterrupted,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadTempDiscarded => (
            GuardianFactId::DownloadTempDiscarded,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadTempWriteFailed | ExecutionFactKind::FileTempLeftover => {
            (GuardianFactId::TempFileLeftover, GuardianDomain::Filesystem)
        }
        ExecutionFactKind::DownloadWrittenToTemp => (
            GuardianFactId::DownloadWrittenToTemp,
            GuardianDomain::Download,
        ),
        ExecutionFactKind::DownloadPromotionFailed => (
            GuardianFactId::AtomicPromotionFailed,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::DownloadPromoted | ExecutionFactKind::FilePromoted => (
            GuardianFactId::AtomicPromotionCompleted,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::FileCorrupt => {
            (GuardianFactId::ManagedFileCorrupt, GuardianDomain::Unknown)
        }
        ExecutionFactKind::FileLocked => {
            (GuardianFactId::FilesystemLocked, GuardianDomain::Filesystem)
        }
        ExecutionFactKind::FileOwnershipUnknown => {
            (GuardianFactId::OwnershipUnknown, GuardianDomain::Unknown)
        }
        ExecutionFactKind::FilePermissionDenied => (
            GuardianFactId::FilesystemPermissionDenied,
            GuardianDomain::Filesystem,
        ),
        ExecutionFactKind::FileQuarantined => {
            (GuardianFactId::ArtifactQuarantined, GuardianDomain::Library)
        }
        ExecutionFactKind::FileWrittenToTemp => {
            (GuardianFactId::FileWrittenToTemp, GuardianDomain::Library)
        }
        ExecutionFactKind::InstallDependencyFailed => (
            GuardianFactId::InstallDependencyFailed,
            GuardianDomain::Install,
        ),
        ExecutionFactKind::RuntimeCorrupt => (
            GuardianFactId::ManagedRuntimeCorrupt,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeJavaOverrideEmpty => {
            (GuardianFactId::JavaOverrideEmpty, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel => (
            GuardianFactId::JavaOverrideUndefinedSentinel,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeMissingExecutable => {
            if fact
                .target
                .as_ref()
                .is_some_and(|target| target.ownership == OwnershipClass::UserOwned)
            {
                (GuardianFactId::JavaOverrideMissing, GuardianDomain::Runtime)
            } else {
                (
                    GuardianFactId::ManagedRuntimeMissing,
                    GuardianDomain::Runtime,
                )
            }
        }
        ExecutionFactKind::RuntimeProbeFailed => {
            (GuardianFactId::JavaProbeFailed, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeReadyMarkerMissing => (
            GuardianFactId::ManagedRuntimeReadyMarkerMissing,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeRepairApplied => (
            GuardianFactId::ManagedRuntimeRepairApplied,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeRosettaRequired => (
            GuardianFactId::ManagedRuntimeRosettaRequired,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeUnavailableForPlatform => (
            GuardianFactId::ManagedRuntimeUnavailableForPlatform,
            GuardianDomain::Runtime,
        ),
        ExecutionFactKind::RuntimeWrongMajor => {
            (GuardianFactId::JavaMajorMismatch, GuardianDomain::Runtime)
        }
        ExecutionFactKind::RuntimeWrongUpdate => {
            (GuardianFactId::JavaUpdateTooOld, GuardianDomain::Runtime)
        }
        ExecutionFactKind::JvmArgsEmpty => (GuardianFactId::JvmArgsEmpty, GuardianDomain::Jvm),
        ExecutionFactKind::JvmArgsParseFailed => {
            (GuardianFactId::JvmArgsParseFailed, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgReservedLauncherFlag => (
            GuardianFactId::JvmArgReservedLauncherFlag,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgMemoryConflict => {
            (GuardianFactId::JvmArgMemoryConflict, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgUnsupportedGc => {
            (GuardianFactId::JvmArgUnsupportedGc, GuardianDomain::Jvm)
        }
        ExecutionFactKind::JvmArgUnlockOrderInvalid => (
            GuardianFactId::JvmArgUnlockOrderInvalid,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgUnsafeClasspathOverride => (
            GuardianFactId::JvmArgUnsafeClasspathOverride,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgUnsafeNativePathOverride => (
            GuardianFactId::JvmArgUnsafeNativePathOverride,
            GuardianDomain::Jvm,
        ),
        ExecutionFactKind::JvmArgAgentOverride => {
            (GuardianFactId::JvmArgAgentOverride, GuardianDomain::Jvm)
        }
        ExecutionFactKind::LaunchCommandInvalid => {
            (GuardianFactId::LaunchCommandInvalid, GuardianDomain::Launch)
        }
        ExecutionFactKind::LaunchCommandPrepared => (
            GuardianFactId::LaunchCommandPrepared,
            GuardianDomain::Launch,
        ),
        ExecutionFactKind::ProcessSpawned => {
            (GuardianFactId::ProcessSpawned, GuardianDomain::Session)
        }
        ExecutionFactKind::ProcessStopIntent => (
            GuardianFactId::LauncherStopRequested,
            GuardianDomain::Session,
        ),
        ExecutionFactKind::ProcessKilled | ExecutionFactKind::ProcessWatchdogAction => (
            GuardianFactId::WatchdogKilledProcess,
            GuardianDomain::Session,
        ),
        ExecutionFactKind::ProcessExitCode => (exit_code_fact_id(fact), GuardianDomain::Session),
        ExecutionFactKind::ProcessBootEvidence => {
            (GuardianFactId::BootMarkerObserved, GuardianDomain::Session)
        }
        ExecutionFactKind::ProcessExited => {
            (GuardianFactId::ProcessExited, GuardianDomain::Session)
        }
        ExecutionFactKind::PrimitiveRefused => {
            (GuardianFactId::PrimitiveRefused, GuardianDomain::Unknown)
        }
        ExecutionFactKind::ProviderDataInvalid => {
            (GuardianFactId::ProviderDataInvalid, GuardianDomain::Network)
        }
        ExecutionFactKind::RollbackAvailable => {
            (GuardianFactId::RollbackAvailable, GuardianDomain::Unknown)
        }
        ExecutionFactKind::RollbackUnavailable => {
            (GuardianFactId::RollbackUnavailable, GuardianDomain::Unknown)
        }
    };
    (id, domain, reliability_for_execution_fact(fact.kind))
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

fn exit_code_fact_id(fact: &ExecutionFact) -> GuardianFactId {
    let exit_code = fact
        .fields
        .iter()
        .find(|field| field.key == "exit_code")
        .and_then(|field| field.value.parse::<i32>().ok());
    match exit_code {
        Some(0) => GuardianFactId::ExitCodeZero,
        Some(_) => GuardianFactId::ExitCodeNonzero,
        None => GuardianFactId::ExitCodeUnknown,
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
