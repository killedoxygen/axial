use crate::guardian::{
    FactReliability, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
    GuardianSeverity,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use croopor_launcher::{LaunchReadiness, LaunchReadinessReasonId, LaunchReadinessSeverity};

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
        .filter_map(|reason| {
            let id = readiness_guardian_fact_id(reason.id)?;
            Some(GuardianFact {
                operation_id: None,
                id: GuardianFactId::new(id),
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
            })
        })
        .collect()
}

fn readiness_guardian_fact_id(reason: LaunchReadinessReasonId) -> Option<&'static str> {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing => Some("version_json_missing"),
        LaunchReadinessReasonId::ParentVersionMissing => Some("parent_version_missing"),
        LaunchReadinessReasonId::IncompleteInstall => Some("incomplete_install"),
        LaunchReadinessReasonId::ClientJarMissing => Some("client_jar_missing"),
        LaunchReadinessReasonId::ClientJarCorrupt => Some("artifact_checksum_mismatch"),
        LaunchReadinessReasonId::LibrariesMissing => Some("libraries_missing"),
        LaunchReadinessReasonId::LibrariesCorrupt => Some("artifact_checksum_mismatch"),
        LaunchReadinessReasonId::AssetIndexMissing => Some("asset_index_missing"),
        LaunchReadinessReasonId::AssetIndexCorrupt => Some("artifact_checksum_mismatch"),
        LaunchReadinessReasonId::ManagedRuntimeMissing => Some("managed_runtime_missing"),
        LaunchReadinessReasonId::JavaOverrideMissing => Some("java_override_missing"),
    }
}

fn readiness_guardian_domain(reason: LaunchReadinessReasonId) -> GuardianDomain {
    match reason {
        LaunchReadinessReasonId::ManagedRuntimeMissing
        | LaunchReadinessReasonId::JavaOverrideMissing => GuardianDomain::Runtime,
        LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesCorrupt
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
        LaunchReadinessReasonId::JavaOverrideMissing => OwnershipClass::UserOwned,
        _ => OwnershipClass::LauncherManaged,
    }
}

fn readiness_guardian_target_kind(reason: LaunchReadinessReasonId) -> TargetKind {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing
        | LaunchReadinessReasonId::ParentVersionMissing
        | LaunchReadinessReasonId::IncompleteInstall => TargetKind::Version,
        LaunchReadinessReasonId::ClientJarMissing
        | LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesMissing
        | LaunchReadinessReasonId::LibrariesCorrupt
        | LaunchReadinessReasonId::AssetIndexMissing
        | LaunchReadinessReasonId::AssetIndexCorrupt => TargetKind::Artifact,
        LaunchReadinessReasonId::ManagedRuntimeMissing => TargetKind::Runtime,
        LaunchReadinessReasonId::JavaOverrideMissing => TargetKind::Config,
    }
}

fn readiness_guardian_target_id(reason: LaunchReadinessReasonId) -> &'static str {
    match reason {
        LaunchReadinessReasonId::VersionJsonMissing => "version_json_missing",
        LaunchReadinessReasonId::ParentVersionMissing => "parent_version_missing",
        LaunchReadinessReasonId::IncompleteInstall => "incomplete_install",
        LaunchReadinessReasonId::ClientJarMissing | LaunchReadinessReasonId::ClientJarCorrupt => {
            "client_jar"
        }
        LaunchReadinessReasonId::LibrariesMissing | LaunchReadinessReasonId::LibrariesCorrupt => {
            "libraries"
        }
        LaunchReadinessReasonId::AssetIndexMissing | LaunchReadinessReasonId::AssetIndexCorrupt => {
            "asset_index"
        }
        LaunchReadinessReasonId::ManagedRuntimeMissing => "managed_runtime",
        LaunchReadinessReasonId::JavaOverrideMissing => "explicit_java_override",
    }
}

fn readiness_guardian_fact_reliability(reason: LaunchReadinessReasonId) -> FactReliability {
    match reason {
        LaunchReadinessReasonId::IncompleteInstall => FactReliability::DirectStructured,
        LaunchReadinessReasonId::ClientJarCorrupt
        | LaunchReadinessReasonId::LibrariesCorrupt
        | LaunchReadinessReasonId::AssetIndexCorrupt => FactReliability::ExactClassifier,
        _ => FactReliability::ExpectedMarkerAbsence,
    }
}
