use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::guardian::{
    FactReliability, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
    GuardianSeverity,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_launcher::{LaunchReadiness, LaunchReadinessReasonId, LaunchReadinessSeverity};

pub(super) fn append_integrity_readiness_reasons(
    readiness: &mut LaunchReadiness,
    facts: &[ExecutionFact],
) {
    for fact in facts {
        let Some(reason) = integrity_readiness_reason(fact) else {
            continue;
        };
        if !readiness
            .reasons
            .iter()
            .any(|existing| existing.id == reason.id)
        {
            readiness.reasons.push(reason);
        }
    }
    readiness.launchable = readiness
        .reasons
        .iter()
        .all(|reason| reason.severity != LaunchReadinessSeverity::Blocking);
}

fn integrity_readiness_reason(
    fact: &ExecutionFact,
) -> Option<axial_launcher::LaunchReadinessReason> {
    let corrupt = match fact.kind {
        ExecutionFactKind::ArtifactMissing => false,
        ExecutionFactKind::ArtifactSizeDrift => true,
        ExecutionFactKind::RuntimeMissingExecutable
        | ExecutionFactKind::RuntimeReadyMarkerMissing => {
            return Some(axial_launcher::LaunchReadinessReason {
                id: LaunchReadinessReasonId::ManagedRuntimeMissing,
                severity: LaunchReadinessSeverity::Recoverable,
                message: "Managed Java runtime is missing and will be prepared before launch.",
            });
        }
        ExecutionFactKind::FilePermissionDenied | ExecutionFactKind::PrimitiveRefused => {
            return Some(axial_launcher::LaunchReadinessReason {
                id: LaunchReadinessReasonId::IncompleteInstall,
                severity: LaunchReadinessSeverity::Blocking,
                message: "Installation verification could not finish. Try launching again shortly.",
            });
        }
        _ => return None,
    };
    let kind = fact
        .fields
        .iter()
        .find(|field| field.key == "artifact_kind")?
        .value
        .as_str();
    let (id, severity, message) = match (kind, corrupt) {
        ("version_metadata", false) => (
            LaunchReadinessReasonId::VersionJsonMissing,
            LaunchReadinessSeverity::Blocking,
            "Installed version metadata is missing. Install this version before launching.",
        ),
        ("version_metadata", true) => (
            LaunchReadinessReasonId::VersionJsonMissing,
            LaunchReadinessSeverity::Blocking,
            "Installed version metadata is invalid. Repair this version before launching.",
        ),
        ("client_jar", false) => (
            LaunchReadinessReasonId::ClientJarMissing,
            LaunchReadinessSeverity::Blocking,
            "Client game files are missing. Install this version before launching.",
        ),
        ("client_jar", true) => (
            LaunchReadinessReasonId::ClientJarCorrupt,
            LaunchReadinessSeverity::Blocking,
            "Client game files are corrupt. Repair this version before launching.",
        ),
        ("library" | "native_library" | "log_config", false) => (
            LaunchReadinessReasonId::LibrariesMissing,
            LaunchReadinessSeverity::Blocking,
            "Required libraries are missing. Install this version before launching.",
        ),
        ("library" | "native_library" | "log_config", true) => (
            LaunchReadinessReasonId::LibrariesCorrupt,
            LaunchReadinessSeverity::Blocking,
            "Required libraries are corrupt. Repair this version before launching.",
        ),
        ("asset_index", false) => (
            LaunchReadinessReasonId::AssetIndexMissing,
            LaunchReadinessSeverity::Blocking,
            "Asset index is missing. Install this version before launching.",
        ),
        ("asset_index", true) => (
            LaunchReadinessReasonId::AssetIndexCorrupt,
            LaunchReadinessSeverity::Blocking,
            "Asset index is corrupt. Repair this version before launching.",
        ),
        ("runtime_manifest_proof" | "runtime_ready_marker" | "runtime_executable", _) => (
            LaunchReadinessReasonId::ManagedRuntimeMissing,
            LaunchReadinessSeverity::Recoverable,
            "Managed Java runtime is missing and will be prepared before launch.",
        ),
        _ => return None,
    };
    Some(axial_launcher::LaunchReadinessReason {
        id,
        severity,
        message,
    })
}

pub(super) fn readiness_has_managed_runtime_missing(readiness: &LaunchReadiness) -> bool {
    readiness
        .reasons
        .iter()
        .any(|reason| reason.id == LaunchReadinessReasonId::ManagedRuntimeMissing)
}

pub(super) fn readiness_guardian_facts(readiness: &LaunchReadiness) -> Vec<GuardianFact> {
    readiness
        .reasons
        .iter()
        .map(|reason| {
            let id = readiness_guardian_fact_id(reason.id);
            GuardianFact {
                operation_id: None,
                id,
                domain: readiness_guardian_domain(reason.id),
                phase: OperationPhase::Validating,
                reliability: readiness_guardian_fact_reliability(reason.id),
                severity: Some(readiness_guardian_severity(reason.severity)),
                confidence: Some(GuardianConfidence::Confirmed),
                ownership: readiness_guardian_ownership(reason.id),
                target: Some(TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    readiness_guardian_target_kind(reason.id),
                    readiness_guardian_target_id(reason.id),
                    readiness_guardian_ownership(reason.id),
                )),
                fields: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn readiness_guardian_facts_for_coverage(
    readiness: &LaunchReadiness,
) -> Vec<GuardianFact> {
    readiness_guardian_facts(readiness)
}

fn readiness_guardian_fact_id(reason: LaunchReadinessReasonId) -> GuardianFactId {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => {
            GuardianFactId::InstalledVersionsDegraded
        }
        LaunchReadinessReasonId::VersionJsonMissing => GuardianFactId::VersionJsonMissing,
        LaunchReadinessReasonId::ParentVersionMissing => GuardianFactId::ParentVersionMissing,
        LaunchReadinessReasonId::IncompleteInstall => GuardianFactId::IncompleteInstall,
        LaunchReadinessReasonId::ClientJarMissing => GuardianFactId::ClientJarMissing,
        LaunchReadinessReasonId::ClientJarCorrupt => GuardianFactId::ArtifactChecksumMismatch,
        LaunchReadinessReasonId::LibrariesMissing => GuardianFactId::LibrariesMissing,
        LaunchReadinessReasonId::LibrariesCorrupt => GuardianFactId::ArtifactChecksumMismatch,
        LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt => {
            GuardianFactId::LauncherManagedArtifactSignatureCorruption
        }
        LaunchReadinessReasonId::AssetIndexMissing => GuardianFactId::AssetIndexMissing,
        LaunchReadinessReasonId::AssetIndexCorrupt => GuardianFactId::ArtifactChecksumMismatch,
        LaunchReadinessReasonId::ManagedRuntimeMissing => GuardianFactId::ManagedRuntimeMissing,
        LaunchReadinessReasonId::JavaOverrideMissing => GuardianFactId::JavaOverrideMissing,
    }
}

fn readiness_guardian_domain(reason: LaunchReadinessReasonId) -> GuardianDomain {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => GuardianDomain::Install,
        LaunchReadinessReasonId::ManagedRuntimeMissing
        | LaunchReadinessReasonId::JavaOverrideMissing => GuardianDomain::Runtime,
        LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesCorrupt
        | LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
        | LaunchReadinessReasonId::AssetIndexCorrupt => GuardianDomain::Download,
        _ => GuardianDomain::Install,
    }
}

fn readiness_guardian_severity(severity: LaunchReadinessSeverity) -> GuardianSeverity {
    match severity {
        LaunchReadinessSeverity::Blocking => GuardianSeverity::Blocking,
        LaunchReadinessSeverity::Recoverable => GuardianSeverity::Recoverable,
    }
}

fn readiness_guardian_ownership(reason: LaunchReadinessReasonId) -> OwnershipClass {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => OwnershipClass::LauncherManaged,
        LaunchReadinessReasonId::JavaOverrideMissing => OwnershipClass::UserOwned,
        _ => OwnershipClass::LauncherManaged,
    }
}

fn readiness_guardian_target_kind(reason: LaunchReadinessReasonId) -> TargetKind {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => TargetKind::Version,
        LaunchReadinessReasonId::VersionJsonMissing
        | LaunchReadinessReasonId::ParentVersionMissing
        | LaunchReadinessReasonId::IncompleteInstall => TargetKind::Version,
        LaunchReadinessReasonId::ClientJarMissing
        | LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesMissing
        | LaunchReadinessReasonId::LibrariesCorrupt
        | LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
        | LaunchReadinessReasonId::AssetIndexMissing
        | LaunchReadinessReasonId::AssetIndexCorrupt => TargetKind::Artifact,
        LaunchReadinessReasonId::ManagedRuntimeMissing => TargetKind::Runtime,
        LaunchReadinessReasonId::JavaOverrideMissing => TargetKind::Config,
    }
}

fn readiness_guardian_target_id(reason: LaunchReadinessReasonId) -> &'static str {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => "installed_versions",
        LaunchReadinessReasonId::VersionJsonMissing => "version_json_missing",
        LaunchReadinessReasonId::ParentVersionMissing => "parent_version_missing",
        LaunchReadinessReasonId::IncompleteInstall => "incomplete_install",
        LaunchReadinessReasonId::ClientJarMissing | LaunchReadinessReasonId::ClientJarCorrupt => {
            "client_jar"
        }
        LaunchReadinessReasonId::LibrariesMissing | LaunchReadinessReasonId::LibrariesCorrupt => {
            "libraries"
        }
        LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt => "launcher_managed_jars",
        LaunchReadinessReasonId::AssetIndexMissing | LaunchReadinessReasonId::AssetIndexCorrupt => {
            "asset_index"
        }
        LaunchReadinessReasonId::ManagedRuntimeMissing => "managed_runtime",
        LaunchReadinessReasonId::JavaOverrideMissing => "explicit_java_override",
    }
}

fn readiness_guardian_fact_reliability(reason: LaunchReadinessReasonId) -> FactReliability {
    match reason {
        LaunchReadinessReasonId::InstalledVersionsDegraded => FactReliability::DirectStructured,
        LaunchReadinessReasonId::IncompleteInstall => FactReliability::DirectStructured,
        LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesCorrupt
        | LaunchReadinessReasonId::LauncherManagedArtifactSignatureCorrupt
        | LaunchReadinessReasonId::AssetIndexCorrupt => FactReliability::ExactClassifier,
        _ => FactReliability::ExpectedMarkerAbsence,
    }
}
