//! Observability and evidence system boundary.
//!
//! Observability owns structured events, local evidence, proof records,
//! redaction scopes, retention, and future telemetry export boundaries.

use crate::state::contracts::{
    CommandKind, OperationId, RollbackState, StabilizationSystem, TargetDescriptor,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationEvent {
    pub operation_id: OperationId,
    pub source: StabilizationSystem,
    pub command: Option<CommandKind>,
    pub stage: OperationEventStage,
    pub severity: EventSeverity,
    pub target: Option<TargetDescriptor>,
    pub fields: Vec<EvidenceField>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationEventStage {
    Started,
    Progress,
    ArtifactVerified,
    RetryScheduled,
    RepairApplied,
    ProcessSpawned,
    ProcessExited,
    Completed,
    Failed,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventSeverity {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceField {
    pub key: String,
    pub value: String,
    pub sensitivity: EvidenceSensitivity,
}

impl EvidenceField {
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
        sensitivity: EvidenceSensitivity,
    ) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            sensitivity,
        }
    }

    pub fn value_for(&self, audience: RedactionAudience) -> Option<&str> {
        audience
            .allows(self.sensitivity)
            .then_some(self.value.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EvidenceSensitivity {
    Public,
    Internal,
    Sensitive,
    Secret,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub operation_id: Option<OperationId>,
    pub kind: EvidenceKind,
    pub source: StabilizationSystem,
    pub target: Option<TargetDescriptor>,
    pub fields: Vec<EvidenceField>,
    pub retention: RetentionClass,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformanceProofRecord {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub health: String,
    pub rollback: RollbackState,
    pub fields: Vec<EvidenceField>,
    pub retention: RetentionClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EvidenceKind {
    CommandEvidence,
    OperationTrace,
    GuardianEvidence,
    PerformanceProof,
    SessionEvidence,
    LocalLog,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RedactionAudience {
    InternalLocal,
    UserVisible,
    ExportableProof,
    TelemetryExport,
}

impl RedactionAudience {
    pub fn allows(self, sensitivity: EvidenceSensitivity) -> bool {
        match sensitivity {
            EvidenceSensitivity::Public => true,
            EvidenceSensitivity::Internal => matches!(self, Self::InternalLocal),
            EvidenceSensitivity::Sensitive => matches!(self, Self::InternalLocal),
            EvidenceSensitivity::Secret => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RetentionClass {
    CurrentOperation,
    RecentHistory,
    Proof,
    FailureMemory,
}

pub fn performance_health_proof_record(
    operation_id: Option<OperationId>,
    target: TargetDescriptor,
    health: impl AsRef<str>,
    rollback: RollbackState,
    fields: Vec<(&str, String)>,
) -> PerformanceProofRecord {
    let health = sanitize_evidence_token(health.as_ref(), RedactionAudience::ExportableProof, 48)
        .unwrap_or_else(|| "redacted".to_string());
    let fields = fields
        .into_iter()
        .map(|(key, value)| {
            let value = sanitize_evidence_token(&value, RedactionAudience::ExportableProof, 96)
                .unwrap_or_else(|| "redacted".to_string());
            EvidenceField::new(key, value, EvidenceSensitivity::Public)
        })
        .collect();
    PerformanceProofRecord {
        operation_id,
        target,
        health,
        rollback,
        fields,
        retention: RetentionClass::Proof,
    }
}

pub fn sanitize_evidence_token(
    value: &str,
    audience: RedactionAudience,
    max_chars: usize,
) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().count() > max_chars
        || evidence_text_looks_sensitive(value)
        || !value.chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        })
    {
        return None;
    }

    audience
        .allows(EvidenceSensitivity::Public)
        .then(|| value.to_string())
}

pub fn sanitize_evidence_text(
    value: &str,
    audience: RedactionAudience,
    max_chars: usize,
) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().count() > max_chars
        || evidence_text_looks_sensitive(&value)
    {
        return None;
    }

    audience
        .allows(EvidenceSensitivity::Public)
        .then_some(value)
}

pub fn evidence_text_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.contains('/') || value.contains('\\') {
        return true;
    }
    if lower.contains(".jar")
        || lower.contains(".exe")
        || lower.contains(".dll")
        || lower.contains(".dylib")
        || lower.contains(".so")
    {
        return true;
    }
    if lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.starts_with("-d")
        || lower.contains(" -d")
        || lower.contains("--access")
        || lower.contains("--username")
        || lower.contains("--uuid")
        || lower.contains("--xuid")
        || lower.contains("--user_properties")
        || lower.contains("--classpath")
        || lower.contains(" -cp ")
        || lower.contains(" -classpath ")
    {
        return true;
    }
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username=")
        || lower.contains("xuid=")
        || lower.contains("bearer ")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }
    if looks_like_jwt(value) || has_long_secret_like_run(value) {
        return true;
    }

    false
}

fn looks_like_jwt(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        let token = token.trim_matches(|value: char| {
            !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        });
        let parts = token.split('.').collect::<Vec<_>>();
        parts.len() >= 3
            && parts.iter().take(3).all(|part| {
                part.len() >= 12
                    && part
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
            })
    })
}

fn has_long_secret_like_run(value: &str) -> bool {
    value
        .split(|value: char| !(value.is_ascii_alphanumeric() || matches!(value, '-' | '_')))
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::{
        EvidenceField, EvidenceKind, EvidenceRecord, EvidenceSensitivity, RedactionAudience,
        RetentionClass, performance_health_proof_record, sanitize_evidence_text,
        sanitize_evidence_token,
    };
    use crate::state::contracts::{
        OperationId, OwnershipClass, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};

    #[test]
    fn evidence_field_visibility_honors_redaction_audience() {
        let public = EvidenceField::new("stage", "launching", EvidenceSensitivity::Public);
        let internal = EvidenceField::new("path", "managed_runtime", EvidenceSensitivity::Internal);
        let sensitive = EvidenceField::new(
            "jvm_arg",
            "explicit_jvm_arg",
            EvidenceSensitivity::Sensitive,
        );
        let secret = EvidenceField::new("token", "secret", EvidenceSensitivity::Secret);

        assert_eq!(
            public.value_for(RedactionAudience::UserVisible),
            Some("launching")
        );
        assert_eq!(
            internal.value_for(RedactionAudience::InternalLocal),
            Some("managed_runtime")
        );
        assert_eq!(internal.value_for(RedactionAudience::ExportableProof), None);
        assert_eq!(
            sensitive.value_for(RedactionAudience::InternalLocal),
            Some("explicit_jvm_arg")
        );
        assert_eq!(
            sensitive.value_for(RedactionAudience::TelemetryExport),
            None
        );
        assert_eq!(secret.value_for(RedactionAudience::InternalLocal), None);
    }

    #[test]
    fn exportable_evidence_sanitizers_drop_sensitive_fragments() {
        assert_eq!(
            sanitize_evidence_token("managed_launch", RedactionAudience::ExportableProof, 96),
            Some("managed_launch".to_string())
        );
        assert_eq!(
            sanitize_evidence_text(
                "Guardian applied a bounded fallback.",
                RedactionAudience::ExportableProof,
                180,
            ),
            Some("Guardian applied a bounded fallback.".to_string())
        );

        for sensitive in [
            "/home/alice/.minecraft",
            r"C:\Users\Alice\AppData\Local\java.exe",
            "-Xmx8192M",
            "-Dauth.username=Player",
            "--accessToken raw-secret",
            "provider_payload={\"token\":\"secret\"}",
            "eyJheader123456789.eyJpayload123456789.signature123456789",
            "account_id=abc",
            "username=SecretPlayer",
        ] {
            assert_eq!(
                sanitize_evidence_text(sensitive, RedactionAudience::ExportableProof, 180),
                None,
                "sensitive text survived: {sensitive}"
            );
            assert_eq!(
                sanitize_evidence_token(sensitive, RedactionAudience::ExportableProof, 96),
                None,
                "sensitive token survived: {sensitive}"
            );
        }
    }

    #[test]
    fn evidence_record_carries_owned_target_without_raw_path() {
        let target = classify_current_artifact(
            CurrentArtifact::UserJavaOverride,
            r"C:\Users\Alice\AppData\Local\java.exe",
        )
        .target;
        let record = EvidenceRecord {
            operation_id: Some(OperationId::new("operation-1")),
            kind: EvidenceKind::CommandEvidence,
            source: StabilizationSystem::Observability,
            target: Some(target),
            fields: vec![EvidenceField::new(
                "summary",
                "custom java path was rejected",
                EvidenceSensitivity::Public,
            )],
            retention: RetentionClass::CurrentOperation,
        };

        let encoded = serde_json::to_string(&record).expect("serialize evidence record");

        assert_eq!(
            record.target.as_ref().map(|target| target.ownership),
            Some(OwnershipClass::UserOwned)
        );
        assert!(encoded.contains("custom_java_path"));
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("java.exe"));
        assert!(!encoded.contains(r"C:\"));
    }

    #[test]
    fn performance_health_proof_bounds_exportable_fields() {
        let proof = performance_health_proof_record(
            Some(OperationId::new("operation-1")),
            TargetDescriptor::new(
                StabilizationSystem::Performance,
                TargetKind::PerformanceComposition,
                r"C:\Users\Alice\.minecraft\mods\sodium.jar",
                OwnershipClass::CompositionManaged,
            ),
            "degraded",
            RollbackState::Available,
            vec![
                ("composition_id", "family-f-fabric-core".to_string()),
                ("tier", "core".to_string()),
                ("warning", "-Xmx8192M".to_string()),
                ("provider", "{\"token\":\"secret\"}".to_string()),
            ],
        );

        let encoded = serde_json::to_string(&proof).expect("serialize performance proof");

        assert_eq!(proof.health, "degraded");
        assert_eq!(proof.rollback, RollbackState::Available);
        assert_eq!(proof.target.id, "target");
        assert!(encoded.contains("family-f-fabric-core"));
        assert!(encoded.contains("redacted"));
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("sodium.jar"));
        assert!(!encoded.contains("-Xmx"));
        assert!(!encoded.contains("secret"));
    }
}
