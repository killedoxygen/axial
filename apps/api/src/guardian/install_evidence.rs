//! Guardian-facing install artifact evidence.
//!
//! This module adapts structured install/download failures into Guardian facts.
//! It does not parse route error strings, choose providers, repair files, or
//! change install progress responses.

use super::{
    GuardianFact, GuardianMode, SafetyCase, build_safety_case, guardian_fact_from_execution,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use croopor_minecraft::download::{
    ExecutionDownloadFact as MinecraftDownloadFact,
    ExecutionDownloadFactKind as MinecraftDownloadFactKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianInstallArtifactFailureKind {
    ChecksumMismatch,
    SizeMismatch,
    ArtifactMissing,
    MetadataInvalid,
    ProviderFailure,
    NetworkFailure,
    PermissionDenied,
    OwnershipRefused,
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

pub fn install_artifact_failure_guardian_fact(
    evidence: &GuardianInstallArtifactFailureEvidence,
    phase: OperationPhase,
) -> GuardianFact {
    let fact = ExecutionFact {
        operation_id: evidence.operation_id.clone(),
        kind: execution_kind_for_install_failure(evidence.kind),
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
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
    let evidence = GuardianInstallArtifactFailureEvidence {
        operation_id,
        target_id: fact.target.clone(),
        ownership,
        kind,
        fields: fact.fields.clone(),
    };
    Some(evidence)
}

pub fn install_artifact_failure_safety_case(
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

fn install_failure_kind_for_minecraft_download_fact(
    kind: MinecraftDownloadFactKind,
) -> Option<GuardianInstallArtifactFailureKind> {
    match kind {
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
        MinecraftDownloadFactKind::PermissionFailure
        | MinecraftDownloadFactKind::PromoteFailed
        | MinecraftDownloadFactKind::TempWriteFailed => {
            Some(GuardianInstallArtifactFailureKind::PermissionDenied)
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
        GuardianInstallArtifactFailureKind::OwnershipRefused => ExecutionFactKind::PrimitiveRefused,
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
    key.contains("user")
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
        install_artifact_failure_from_minecraft_download_fact,
        install_artifact_failure_guardian_fact, install_artifact_failure_safety_case,
    };
    use crate::guardian::{GuardianActionKind, GuardianMode};
    use crate::state::contracts::{OperationId, OperationPhase, OwnershipClass};
    use croopor_minecraft::download::{
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
            .find(|diagnosis| diagnosis.id.as_str() == "launcher_managed_artifact_corrupt")
            .expect("corruption diagnosis");
        assert!(
            diagnosis
                .candidate_actions
                .contains(&GuardianActionKind::Repair)
        );
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
                GuardianInstallArtifactFailureKind::OwnershipRefused,
                "artifact_ownership_unsafe",
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
                    .any(|diagnosis| diagnosis.id.as_str() == diagnosis_id),
                "missing diagnosis {diagnosis_id} for {kind:?}: {:?}",
                safety_case.diagnoses
            );
        }
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
