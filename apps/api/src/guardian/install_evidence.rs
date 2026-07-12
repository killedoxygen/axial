//! Guardian-facing install artifact evidence.
//!
//! This module adapts structured install/download failures into Guardian facts.
//! It does not parse route error strings, choose providers, repair files, or
//! change install progress responses.

use super::GuardianPolicyContext;
use super::{
    DiagnosisId, GuardianActionKind, GuardianCopyRequest, GuardianDecision, GuardianFact,
    GuardianMode, GuardianUserOutcome, MissingDownload, QuarantineRedownload, RepairAuthorization,
    RepairAuthorizationRejection, SafetyCase, author_guardian_copy,
    authorize_launcher_managed_artifact_repair, authorize_launcher_managed_missing_artifact_repair,
    build_safety_case, decide_guardian_policy, guardian_fact_from_execution,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, evidence_text_looks_sensitive,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_minecraft::download::{
    ExecutionDownloadFact as MinecraftDownloadFact,
    ExecutionDownloadFactKind as MinecraftDownloadFactKind,
};

macro_rules! install_artifact_failure_kinds {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum GuardianInstallArtifactFailureKind {
            $($variant),+
        }

        impl GuardianInstallArtifactFailureKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

install_artifact_failure_kinds! {
    ChecksumMismatch,
    SizeMismatch,
    ArtifactMissing,
    MetadataInvalid,
    ProviderFailure,
    NetworkFailure,
    PermissionDenied,
    TempWriteFailed,
    PromotionFailed,
    DependencyFailed,
    ExecutionFailed,
    ProcessorFailed,
    OwnershipRefused,
    RuntimeRosettaRequired,
    RuntimeUnavailableForPlatform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianInstallArtifactFailureEvidence {
    pub operation_id: Option<OperationId>,
    pub target_id: String,
    pub ownership: OwnershipClass,
    pub kind: GuardianInstallArtifactFailureKind,
    pub fields: Vec<(String, String)>,
}

impl GuardianInstallArtifactFailureEvidence {
    pub fn launcher_managed(
        operation_id: Option<OperationId>,
        target_id: impl Into<String>,
        kind: GuardianInstallArtifactFailureKind,
    ) -> Self {
        Self {
            operation_id,
            target_id: target_id.into(),
            ownership: OwnershipClass::LauncherManaged,
            kind,
            fields: Vec::new(),
        }
    }

    pub fn with_ownership(mut self, ownership: OwnershipClass) -> Self {
        self.ownership = ownership;
        self
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardianInstallFailureOutcome {
    pub diagnosis_id: DiagnosisId,
    pub decision: GuardianActionKind,
    pub user_outcome: GuardianUserOutcome,
}

pub(crate) struct GuardianInstallAssessment {
    decision: GuardianDecision,
    terminal_outcome: Option<GuardianInstallFailureOutcome>,
}

impl GuardianInstallAssessment {
    pub(crate) const fn decision_kind(&self) -> GuardianActionKind {
        self.decision.kind()
    }

    pub(crate) fn repair_target(&self) -> Option<&TargetDescriptor> {
        self.decision
            .action_plan()
            .and_then(|plan| plan.actions.first())
            .and_then(|action| action.target.as_ref())
            .or_else(|| {
                self.decision
                    .action_plan()
                    .and_then(|plan| plan.prerequisite.affected_targets.first())
            })
    }

    pub(crate) fn terminal_outcome(&self) -> Option<&GuardianInstallFailureOutcome> {
        self.terminal_outcome.as_ref()
    }

    pub(crate) fn into_existing_repair_authorization(
        self,
        descriptor: super::GuardianMinecraftArtifactRepairDescriptor,
    ) -> Result<RepairAuthorization<QuarantineRedownload>, RepairAuthorizationRejection> {
        authorize_launcher_managed_artifact_repair(&self.decision, Default::default(), descriptor)
    }

    pub(crate) fn into_missing_repair_authorization(
        self,
        descriptor: super::GuardianMinecraftArtifactRepairDescriptor,
    ) -> Result<RepairAuthorization<MissingDownload>, RepairAuthorizationRejection> {
        authorize_launcher_managed_missing_artifact_repair(
            &self.decision,
            Default::default(),
            descriptor,
        )
    }
}

pub fn install_artifact_failure_guardian_fact(
    evidence: &GuardianInstallArtifactFailureEvidence,
    phase: OperationPhase,
) -> GuardianFact {
    let fact = ExecutionFact {
        operation_id: evidence.operation_id.clone(),
        kind: execution_kind_for_install_failure(evidence.kind),
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            target_kind_for_install_failure(evidence.kind),
            safe_artifact_target_id(&evidence.target_id),
            evidence.ownership,
        )),
        fields: public_safe_install_fields(&evidence.fields),
    };
    guardian_fact_from_execution(&fact, phase)
}

pub fn install_artifact_failure_from_minecraft_download_fact(
    operation_id: Option<OperationId>,
    ownership: OwnershipClass,
    fact: &MinecraftDownloadFact,
) -> Option<GuardianInstallArtifactFailureEvidence> {
    let kind = install_failure_kind_for_minecraft_download_fact(fact.kind)?;
    let ownership = if kind == GuardianInstallArtifactFailureKind::OwnershipRefused
        && ownership == OwnershipClass::LauncherManaged
    {
        OwnershipClass::Unknown
    } else {
        ownership
    };
    let evidence = GuardianInstallArtifactFailureEvidence {
        operation_id,
        target_id: fact.target.clone(),
        ownership,
        kind,
        fields: fact.fields.clone(),
    };
    Some(evidence)
}

pub(crate) fn install_artifact_failure_safety_case(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
) -> SafetyCase {
    let facts = evidence
        .iter()
        .map(|evidence| install_artifact_failure_guardian_fact(evidence, phase))
        .collect::<Vec<_>>();
    build_safety_case(operation_id, mode, phase, &facts)
}

#[cfg(test)]
pub(crate) fn assess_install_artifact_failure(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
) -> Option<GuardianInstallAssessment> {
    assess_install_artifact_failure_with_context(
        operation_id,
        mode,
        phase,
        evidence,
        GuardianPolicyContext::current_operation(),
    )
}

pub(crate) fn assess_install_artifact_failure_with_context(
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    phase: OperationPhase,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    context: GuardianPolicyContext,
) -> Option<GuardianInstallAssessment> {
    if evidence.is_empty() {
        return None;
    }

    let safety_case = install_artifact_failure_safety_case(operation_id, mode, phase, evidence);
    let decision = decide_guardian_policy(&safety_case, context);
    let terminal_outcome = if matches!(
        decision.kind(),
        GuardianActionKind::Allow | GuardianActionKind::RecordOnly | GuardianActionKind::Repair
    ) {
        None
    } else {
        let diagnosis_id = decision
            .action_plan()
            .map(|plan| plan.prerequisite.diagnosis_id)
            .or_else(|| decision.diagnoses().first().copied())?;
        if diagnosis_id == DiagnosisId::LauncherManagedArtifactCorrupt {
            None
        } else {
            let user_outcome = author_guardian_copy(GuardianCopyRequest::install_failure(
                diagnosis_id,
                decision.kind(),
                evidence,
            ))?;
            Some(GuardianInstallFailureOutcome {
                diagnosis_id,
                decision: decision.kind(),
                user_outcome,
            })
        }
    };
    Some(GuardianInstallAssessment {
        decision,
        terminal_outcome,
    })
}

fn install_failure_kind_for_minecraft_download_fact(
    kind: MinecraftDownloadFactKind,
) -> Option<GuardianInstallArtifactFailureKind> {
    match kind {
        MinecraftDownloadFactKind::ArtifactMissing => {
            Some(GuardianInstallArtifactFailureKind::ArtifactMissing)
        }
        MinecraftDownloadFactKind::ChecksumMismatch => {
            Some(GuardianInstallArtifactFailureKind::ChecksumMismatch)
        }
        MinecraftDownloadFactKind::SizeMismatch => {
            Some(GuardianInstallArtifactFailureKind::SizeMismatch)
        }
        MinecraftDownloadFactKind::MetadataInvalid | MinecraftDownloadFactKind::MetadataMissing => {
            Some(GuardianInstallArtifactFailureKind::MetadataInvalid)
        }
        MinecraftDownloadFactKind::ProviderFailure => {
            Some(GuardianInstallArtifactFailureKind::ProviderFailure)
        }
        MinecraftDownloadFactKind::NetworkFailure | MinecraftDownloadFactKind::Interrupted => {
            Some(GuardianInstallArtifactFailureKind::NetworkFailure)
        }
        MinecraftDownloadFactKind::PermissionFailure => {
            Some(GuardianInstallArtifactFailureKind::PermissionDenied)
        }
        MinecraftDownloadFactKind::TempWriteFailed => {
            Some(GuardianInstallArtifactFailureKind::TempWriteFailed)
        }
        MinecraftDownloadFactKind::PromoteFailed => {
            Some(GuardianInstallArtifactFailureKind::PromotionFailed)
        }
        MinecraftDownloadFactKind::OwnershipRefused => {
            Some(GuardianInstallArtifactFailureKind::OwnershipRefused)
        }
        MinecraftDownloadFactKind::ArtifactVerified
        | MinecraftDownloadFactKind::TempDiscarded
        | MinecraftDownloadFactKind::WrittenToTemp
        | MinecraftDownloadFactKind::Promoted => None,
    }
}

fn execution_kind_for_install_failure(
    kind: GuardianInstallArtifactFailureKind,
) -> ExecutionFactKind {
    match kind {
        GuardianInstallArtifactFailureKind::ChecksumMismatch => {
            ExecutionFactKind::DownloadChecksumMismatch
        }
        GuardianInstallArtifactFailureKind::SizeMismatch => ExecutionFactKind::DownloadSizeMismatch,
        GuardianInstallArtifactFailureKind::ArtifactMissing => ExecutionFactKind::ArtifactMissing,
        GuardianInstallArtifactFailureKind::MetadataInvalid => {
            ExecutionFactKind::ProviderDataInvalid
        }
        GuardianInstallArtifactFailureKind::ProviderFailure => {
            ExecutionFactKind::DownloadProviderFailure
        }
        GuardianInstallArtifactFailureKind::NetworkFailure => {
            ExecutionFactKind::DownloadNetworkFailure
        }
        GuardianInstallArtifactFailureKind::PermissionDenied => {
            ExecutionFactKind::FilePermissionDenied
        }
        GuardianInstallArtifactFailureKind::TempWriteFailed => {
            ExecutionFactKind::DownloadTempWriteFailed
        }
        GuardianInstallArtifactFailureKind::PromotionFailed => {
            ExecutionFactKind::DownloadPromotionFailed
        }
        GuardianInstallArtifactFailureKind::DependencyFailed => {
            ExecutionFactKind::InstallDependencyFailed
        }
        GuardianInstallArtifactFailureKind::ExecutionFailed => {
            ExecutionFactKind::InstallExecutionFailed
        }
        GuardianInstallArtifactFailureKind::ProcessorFailed => {
            ExecutionFactKind::InstallProcessorFailed
        }
        GuardianInstallArtifactFailureKind::OwnershipRefused => ExecutionFactKind::PrimitiveRefused,
        GuardianInstallArtifactFailureKind::RuntimeRosettaRequired => {
            ExecutionFactKind::RuntimeRosettaRequired
        }
        GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform => {
            ExecutionFactKind::RuntimeUnavailableForPlatform
        }
    }
}

fn target_kind_for_install_failure(kind: GuardianInstallArtifactFailureKind) -> TargetKind {
    match kind {
        GuardianInstallArtifactFailureKind::RuntimeRosettaRequired
        | GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform => TargetKind::Runtime,
        GuardianInstallArtifactFailureKind::ExecutionFailed
        | GuardianInstallArtifactFailureKind::ProcessorFailed => TargetKind::Version,
        _ => TargetKind::Artifact,
    }
}

fn safe_artifact_target_id(value: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| "install_artifact".to_string())
}

fn public_safe_install_fields(fields: &[(String, String)]) -> Vec<EvidenceField> {
    fields
        .iter()
        .filter_map(|(key, value)| {
            if install_field_key_looks_sensitive(key) {
                return None;
            }
            let key = sanitize_evidence_token(key, RedactionAudience::UserVisible, 32)?;
            let value = sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)?;
            Some(EvidenceField::new(key, value, EvidenceSensitivity::Public))
        })
        .collect()
}

fn install_field_key_looks_sensitive(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    evidence_text_looks_sensitive(&key)
        || key.contains("user")
        || key.contains("account")
        || key.contains("uuid")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("path")
        || key.contains("url")
        || key.contains("arg")
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
        assess_install_artifact_failure, install_artifact_failure_from_minecraft_download_fact,
        install_artifact_failure_guardian_fact, install_artifact_failure_safety_case,
    };
    use crate::guardian::{
        GuardianActionKind, GuardianMinecraftArtifactRepairDescriptor, GuardianMode,
    };
    use crate::state::contracts::{
        OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use axial_minecraft::download::{
        ExecutionDownloadFact as MinecraftDownloadFact,
        ExecutionDownloadFactKind as MinecraftDownloadFactKind,
    };

    #[test]
    fn checksum_failure_maps_to_repairable_corruption_diagnosis() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(OperationId::new("install-operation-1")),
            "minecraft_client_1.21.5",
            GuardianInstallArtifactFailureKind::ChecksumMismatch,
        )
        .with_field("algorithm", "sha1")
        .with_field("url", "https://example.invalid/artifact.jar?token=secret")
        .with_field("path", "/home/alice/.minecraft/versions/1.21.5/1.21.5.jar");

        let fact = install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        assert_eq!(fact.id.as_str(), "artifact_checksum_mismatch");
        assert_eq!(
            fact.target.as_ref().expect("target").id,
            "minecraft_client_1.21.5"
        );
        assert_eq!(fact.fields.len(), 1);
        assert_eq!(fact.fields[0].key, "algorithm");
        assert_eq!(fact.fields[0].value, "sha1");

        let safety_case = install_artifact_failure_safety_case(
            Some(OperationId::new("install-operation-1")),
            GuardianMode::Managed,
            OperationPhase::Downloading,
            &[evidence],
        );
        let diagnosis = safety_case
            .diagnoses
            .iter()
            .find(|diagnosis| diagnosis.id().as_str() == "launcher_managed_artifact_corrupt")
            .expect("corruption diagnosis");
        assert!(
            diagnosis
                .candidate_actions()
                .contains(&GuardianActionKind::Repair)
        );
    }

    #[test]
    fn install_artifact_repair_authorization_is_selected_by_guardian_policy() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(OperationId::new("install-operation-1")),
            "minecraft_client_1.21.5",
            GuardianInstallArtifactFailureKind::ChecksumMismatch,
        );

        let assessment = assess_install_artifact_failure(
            Some(OperationId::new("install-operation-1")),
            GuardianMode::Managed,
            OperationPhase::Downloading,
            &[evidence],
        )
        .expect("Guardian assesses managed artifact failure");
        let _authorization = assessment
            .into_existing_repair_authorization(test_descriptor("minecraft_client_1.21.5"))
            .expect("Guardian policy selects managed artifact repair");
    }

    fn test_descriptor(target_id: &str) -> GuardianMinecraftArtifactRepairDescriptor {
        GuardianMinecraftArtifactRepairDescriptor::for_test(
            TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                target_id,
                OwnershipClass::LauncherManaged,
            ),
            std::path::Path::new("/tmp/axial-test-artifact.jar"),
            "https://example.invalid/artifact.jar",
            "sha1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(128),
            1024,
        )
        .expect("test descriptor")
    }

    #[test]
    fn structured_install_failures_map_to_bounded_diagnoses() {
        let cases = [
            (
                GuardianInstallArtifactFailureKind::SizeMismatch,
                "launcher_managed_artifact_corrupt",
            ),
            (
                GuardianInstallArtifactFailureKind::ArtifactMissing,
                "launcher_managed_artifact_corrupt",
            ),
            (
                GuardianInstallArtifactFailureKind::MetadataInvalid,
                "install_artifact_metadata_invalid",
            ),
            (
                GuardianInstallArtifactFailureKind::ProviderFailure,
                "download_unavailable",
            ),
            (
                GuardianInstallArtifactFailureKind::NetworkFailure,
                "download_unavailable",
            ),
            (
                GuardianInstallArtifactFailureKind::PermissionDenied,
                "filesystem_permission_denied",
            ),
            (
                GuardianInstallArtifactFailureKind::TempWriteFailed,
                "temp_file_write_failed",
            ),
            (
                GuardianInstallArtifactFailureKind::PromotionFailed,
                "atomic_promotion_failed",
            ),
            (
                GuardianInstallArtifactFailureKind::DependencyFailed,
                "install_dependency_failed",
            ),
            (
                GuardianInstallArtifactFailureKind::ExecutionFailed,
                "install_execution_failed",
            ),
            (
                GuardianInstallArtifactFailureKind::ProcessorFailed,
                "install_processor_failed",
            ),
            (
                GuardianInstallArtifactFailureKind::OwnershipRefused,
                "artifact_ownership_unsafe",
            ),
            (
                GuardianInstallArtifactFailureKind::RuntimeRosettaRequired,
                "managed_runtime_rosetta_required",
            ),
            (
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
                "managed_runtime_unavailable_for_platform",
            ),
        ];

        for (kind, diagnosis_id) in cases {
            let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "minecraft_library_org.example.lib.1.0.0",
                kind,
            );
            let safety_case = install_artifact_failure_safety_case(
                None,
                GuardianMode::Managed,
                OperationPhase::Downloading,
                &[evidence],
            );
            assert!(
                safety_case
                    .diagnoses
                    .iter()
                    .any(|diagnosis| diagnosis.id().as_str() == diagnosis_id),
                "missing diagnosis {diagnosis_id} for {kind:?}: {:?}",
                safety_case.diagnoses
            );
        }
    }

    #[test]
    fn provider_and_network_failures_produce_guardian_user_outcome_without_repair() {
        for kind in [
            GuardianInstallArtifactFailureKind::ProviderFailure,
            GuardianInstallArtifactFailureKind::NetworkFailure,
        ] {
            let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(OperationId::new("install-operation-1")),
                "minecraft_client_1.21.5",
                kind,
            )
            .with_field("url", "https://example.invalid/client.jar?token=secret")
            .with_field("provider_payload", "{\"token\":\"secret\"}");

            let assessment = assess_install_artifact_failure(
                Some(OperationId::new("install-operation-1")),
                GuardianMode::Managed,
                OperationPhase::Downloading,
                std::slice::from_ref(&evidence),
            )
            .expect("Guardian assessment");
            let outcome = assessment
                .terminal_outcome()
                .expect("Guardian terminal outcome");

            assert_eq!(outcome.diagnosis_id.as_str(), "download_unavailable");
            assert_eq!(outcome.decision, crate::guardian::GuardianActionKind::Retry);
            assert!(
                outcome
                    .user_outcome
                    .summary()
                    .contains("download failure as retryable")
            );
            let encoded = serde_json::to_string(&outcome.user_outcome)
                .expect("outcome json")
                .to_ascii_lowercase();
            assert!(!encoded.contains("example.invalid"));
            assert!(!encoded.contains("provider_payload"));
            assert!(!encoded.contains("token"));
            assert!(!encoded.contains("secret"));
        }
    }

    #[test]
    fn runtime_unavailable_failure_produces_device_specific_blocking_outcome() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(OperationId::new("install-operation-1")),
            "java_runtime_jre-legacy_mac-os-arm64",
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
        )
        .with_field("component", "jre-legacy")
        .with_field("platform", "mac-os-arm64")
        .with_field("url", "https://example.invalid/runtime.json?token=secret");

        let assessment = assess_install_artifact_failure(
            Some(OperationId::new("install-operation-1")),
            GuardianMode::Managed,
            OperationPhase::Downloading,
            std::slice::from_ref(&evidence),
        )
        .expect("Guardian assessment");
        let outcome = assessment
            .terminal_outcome()
            .expect("Guardian terminal outcome");

        assert_eq!(
            outcome.diagnosis_id.as_str(),
            "managed_runtime_unavailable_for_platform"
        );
        assert_eq!(outcome.decision, crate::guardian::GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.summary(),
            "This Minecraft version needs a Java runtime that is not available for this device."
        );
        assert_eq!(
            outcome.user_outcome.details(),
            vec![
                "Java runtime component jre-legacy is not available for mac-os-arm64.".to_string()
            ]
        );
        assert_eq!(
            outcome.user_outcome.guidance(),
            vec!["This version cannot be installed on this device.".to_string()]
        );

        let fact = install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        assert_eq!(fact.id.as_str(), "managed_runtime_unavailable_for_platform");
        assert_eq!(
            fact.target.as_ref().expect("target").kind,
            TargetKind::Runtime
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| { field.key == "component" && field.value == "jre-legacy" })
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| field.key == "platform" && field.value == "mac-os-arm64")
        );
        let encoded = serde_json::to_string(&outcome.user_outcome)
            .expect("outcome json")
            .to_ascii_lowercase();
        assert!(!encoded.contains("example.invalid"));
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("secret"));
    }

    #[test]
    fn runtime_rosetta_failure_produces_actionable_blocking_outcome() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(OperationId::new("install-operation-1")),
            "java_runtime_jre-legacy_rosetta",
            GuardianInstallArtifactFailureKind::RuntimeRosettaRequired,
        )
        .with_field("component", "jre-legacy")
        .with_field("path", "/Users/alice/.axial/runtime/java");

        let assessment = assess_install_artifact_failure(
            Some(OperationId::new("install-operation-1")),
            GuardianMode::Managed,
            OperationPhase::Downloading,
            std::slice::from_ref(&evidence),
        )
        .expect("Guardian assessment");
        let outcome = assessment
            .terminal_outcome()
            .expect("Guardian terminal outcome");

        assert_eq!(
            outcome.diagnosis_id.as_str(),
            "managed_runtime_rosetta_required"
        );
        assert_eq!(outcome.decision, crate::guardian::GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.summary(),
            "This Minecraft version needs Rosetta 2 on Apple Silicon Macs."
        );
        assert_eq!(
            outcome.user_outcome.details(),
            vec!["Java runtime component jre-legacy needs Rosetta 2 on this Mac.".to_string()]
        );
        assert_eq!(
            outcome.user_outcome.guidance(),
            vec![
                "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.".to_string()
            ]
        );

        let fact = install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        assert_eq!(fact.id.as_str(), "managed_runtime_rosetta_required");
        assert_eq!(
            fact.target.as_ref().expect("target").kind,
            TargetKind::Runtime
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| { field.key == "component" && field.value == "jre-legacy" })
        );
        let encoded = serde_json::to_string(&outcome.user_outcome)
            .expect("outcome json")
            .to_ascii_lowercase();
        assert!(!encoded.contains("users/alice"));
    }

    #[test]
    fn non_repairable_install_safety_failures_block_without_repair() {
        let cases = [
            (
                GuardianInstallArtifactFailureKind::MetadataInvalid,
                "install_artifact_metadata_invalid",
                "provider metadata could not be trusted",
            ),
            (
                GuardianInstallArtifactFailureKind::PermissionDenied,
                "filesystem_permission_denied",
                "could not write launcher-managed files safely",
            ),
            (
                GuardianInstallArtifactFailureKind::TempWriteFailed,
                "temp_file_write_failed",
                "temporary download state could not be written safely",
            ),
            (
                GuardianInstallArtifactFailureKind::PromotionFailed,
                "atomic_promotion_failed",
                "verified download data could not be promoted safely",
            ),
            (
                GuardianInstallArtifactFailureKind::DependencyFailed,
                "install_dependency_failed",
                "required base install failed",
            ),
            (
                GuardianInstallArtifactFailureKind::ExecutionFailed,
                "install_execution_failed",
                "installation could not complete",
            ),
            (
                GuardianInstallArtifactFailureKind::ProcessorFailed,
                "install_processor_failed",
                "installer processor failed",
            ),
            (
                GuardianInstallArtifactFailureKind::OwnershipRefused,
                "artifact_ownership_unsafe",
                "protect user-owned or unknown files",
            ),
            (
                GuardianInstallArtifactFailureKind::RuntimeRosettaRequired,
                "managed_runtime_rosetta_required",
                "Rosetta 2",
            ),
            (
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
                "managed_runtime_unavailable_for_platform",
                "Java runtime that is not available",
            ),
        ];

        for (kind, diagnosis_id, summary_fragment) in cases {
            let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                Some(OperationId::new("install-operation-1")),
                "minecraft_client_1.21.5",
                kind,
            )
            .with_field("path", "/Users/alice/.axial/libraries/secret.jar")
            .with_field("url", "https://example.invalid/client.jar?token=secret")
            .with_field("provider_payload", "{\"token\":\"secret\"}");

            let assessment = assess_install_artifact_failure(
                Some(OperationId::new("install-operation-1")),
                GuardianMode::Managed,
                OperationPhase::Downloading,
                std::slice::from_ref(&evidence),
            )
            .expect("Guardian assessment");
            let outcome = assessment
                .terminal_outcome()
                .expect("Guardian terminal outcome");

            assert_eq!(outcome.diagnosis_id.as_str(), diagnosis_id);
            assert_eq!(outcome.decision, crate::guardian::GuardianActionKind::Block);
            assert!(
                outcome.user_outcome.summary().contains(summary_fragment),
                "{diagnosis_id} summary did not contain expected fragment: {:?}",
                outcome.user_outcome
            );
            if matches!(
                kind,
                GuardianInstallArtifactFailureKind::RuntimeRosettaRequired
                    | GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform
            ) {
                assert_eq!(
                    install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading)
                        .target
                        .as_ref()
                        .expect("target")
                        .kind,
                    TargetKind::Runtime
                );
            }
            let encoded = serde_json::to_string(&outcome.user_outcome)
                .expect("outcome json")
                .to_ascii_lowercase();
            assert!(!encoded.contains("users/alice"));
            assert!(!encoded.contains("example.invalid"));
            assert!(!encoded.contains("provider_payload"));
            assert!(!encoded.contains("token"));
            assert!(!encoded.contains("secret"));
        }
    }

    #[test]
    fn repairable_artifact_corruption_does_not_emit_generic_install_failure_outcome() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(OperationId::new("install-operation-1")),
            "minecraft_client_1.21.5",
            GuardianInstallArtifactFailureKind::ChecksumMismatch,
        );

        let assessment = assess_install_artifact_failure(
            Some(OperationId::new("install-operation-1")),
            GuardianMode::Managed,
            OperationPhase::Downloading,
            &[evidence],
        )
        .expect("Guardian assessment");
        assert!(assessment.terminal_outcome().is_none());
    }

    #[test]
    fn unsafe_target_and_fields_are_redacted() {
        let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
            None,
            r"C:\Users\Alice\AppData\Roaming\.minecraft\libraries\bad.jar",
            GuardianInstallArtifactFailureKind::PermissionDenied,
        )
        .with_ownership(OwnershipClass::Unknown)
        .with_field("username", "Alice")
        .with_field("token", "secret")
        .with_field("phase", "libraries");

        let fact = install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        let encoded = serde_json::to_string(&fact)
            .expect("fact json")
            .to_ascii_lowercase();

        assert_eq!(fact.target.as_ref().expect("target").id, "install_artifact");
        assert_eq!(fact.ownership, OwnershipClass::Unknown);
        assert_eq!(fact.fields.len(), 1);
        assert_eq!(fact.fields[0].key, "phase");
        assert_eq!(fact.fields[0].value, "libraries");
        assert!(!encoded.contains("alice"));
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("appdata"));
        assert!(!encoded.contains("bad.jar"));
    }

    #[test]
    fn minecraft_download_fact_converts_to_guardian_install_evidence() {
        let fact = MinecraftDownloadFact {
            kind: MinecraftDownloadFactKind::ChecksumMismatch,
            target: "minecraft_client_1.21.5".to_string(),
            fields: vec![
                ("algorithm".to_string(), "sha1".to_string()),
                (
                    "url".to_string(),
                    "https://example.invalid/artifact.jar?token=secret".to_string(),
                ),
            ],
        };

        let evidence = install_artifact_failure_from_minecraft_download_fact(
            Some(OperationId::new("install-operation-1")),
            OwnershipClass::LauncherManaged,
            &fact,
        )
        .expect("failure evidence");
        assert_eq!(
            evidence.kind,
            GuardianInstallArtifactFailureKind::ChecksumMismatch
        );
        let guardian_fact =
            install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        let encoded = serde_json::to_string(&guardian_fact)
            .expect("fact json")
            .to_ascii_lowercase();

        assert_eq!(guardian_fact.id.as_str(), "artifact_checksum_mismatch");
        assert_eq!(guardian_fact.fields.len(), 1);
        assert_eq!(guardian_fact.fields[0].key, "algorithm");
        assert!(!encoded.contains("example.invalid"));
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("secret"));
    }

    #[test]
    fn minecraft_download_ownership_refusal_is_protected_when_caller_lacks_ownership_context() {
        let fact = MinecraftDownloadFact {
            kind: MinecraftDownloadFactKind::OwnershipRefused,
            target: "minecraft_client_1.21.5".to_string(),
            fields: Vec::new(),
        };

        let evidence = install_artifact_failure_from_minecraft_download_fact(
            Some(OperationId::new("install-operation-1")),
            OwnershipClass::LauncherManaged,
            &fact,
        )
        .expect("failure evidence");

        assert_eq!(
            evidence.kind,
            GuardianInstallArtifactFailureKind::OwnershipRefused
        );
        assert_eq!(evidence.ownership, OwnershipClass::Unknown);

        let guardian_fact =
            install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
        assert_eq!(guardian_fact.id.as_str(), "primitive_refused");
        assert_eq!(guardian_fact.ownership, OwnershipClass::Unknown);
    }

    #[test]
    fn minecraft_download_temp_and_promotion_failures_keep_distinct_guardian_facts() {
        let cases = [
            (
                MinecraftDownloadFactKind::TempWriteFailed,
                GuardianInstallArtifactFailureKind::TempWriteFailed,
                "temp_file_write_failed",
            ),
            (
                MinecraftDownloadFactKind::PromoteFailed,
                GuardianInstallArtifactFailureKind::PromotionFailed,
                "atomic_promotion_failed",
            ),
        ];

        for (download_kind, failure_kind, fact_id) in cases {
            let fact = MinecraftDownloadFact {
                kind: download_kind,
                target: "minecraft_client_1.21.5".to_string(),
                fields: vec![
                    (
                        "path".to_string(),
                        "/Users/alice/.axial/libraries/secret.jar".to_string(),
                    ),
                    ("phase".to_string(), "promote".to_string()),
                ],
            };

            let evidence = install_artifact_failure_from_minecraft_download_fact(
                Some(OperationId::new("install-operation-1")),
                OwnershipClass::LauncherManaged,
                &fact,
            )
            .expect("failure evidence");

            assert_eq!(evidence.kind, failure_kind);
            let guardian_fact =
                install_artifact_failure_guardian_fact(&evidence, OperationPhase::Downloading);
            assert_eq!(guardian_fact.id.as_str(), fact_id);
            assert!(
                guardian_fact.fields.iter().all(|field| field.key != "path"),
                "{fact_id} leaked sensitive path field: {guardian_fact:?}"
            );
        }
    }

    #[test]
    fn minecraft_download_success_facts_are_not_failure_evidence() {
        for kind in [
            MinecraftDownloadFactKind::ArtifactVerified,
            MinecraftDownloadFactKind::TempDiscarded,
            MinecraftDownloadFactKind::WrittenToTemp,
            MinecraftDownloadFactKind::Promoted,
        ] {
            let fact = MinecraftDownloadFact {
                kind,
                target: "minecraft_client_1.21.5".to_string(),
                fields: Vec::new(),
            };
            assert!(
                install_artifact_failure_from_minecraft_download_fact(
                    None,
                    OwnershipClass::LauncherManaged,
                    &fact,
                )
                .is_none(),
                "{kind:?} should not become failure evidence"
            );
        }
    }
}
