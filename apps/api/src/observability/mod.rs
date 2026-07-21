//! Observability and evidence system boundary.
//!
//! Observability owns structured events, local evidence, proof records,
//! redaction scopes, retention, and the telemetry-safe export boundary.

pub mod telemetry;

use crate::guardian::DiagnosisId;
use crate::state::contracts::{
    CommandKind, OperationId, OperationJournalEntry, OperationOutcome, OperationStatus,
    RollbackState, StabilizationSystem, TargetDescriptor,
};
use serde::{Deserialize, Serialize};

pub const PUBLIC_LOG_LINE_REDACTED: &str = "Log line hidden for privacy.";

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationProofRecord {
    pub operation_id: OperationId,
    pub command: CommandKind,
    pub status: OperationStatus,
    pub outcome: Option<OperationOutcome>,
    pub targets: Vec<TargetDescriptor>,
    pub failure_point: Option<String>,
    pub guardian_diagnosis_ids: Vec<DiagnosisId>,
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

pub fn operation_journal_proof_record(entry: &OperationJournalEntry) -> OperationProofRecord {
    OperationProofRecord {
        operation_id: sanitized_operation_id(entry.operation_id.as_str()),
        command: entry.command,
        status: entry.status,
        outcome: entry.outcome,
        targets: entry
            .targets
            .iter()
            .take(8)
            .map(sanitized_target_descriptor)
            .collect(),
        failure_point: entry
            .failure_point
            .as_deref()
            .and_then(|value| sanitize_proof_identifier(value, 96)),
        guardian_diagnosis_ids: entry
            .guardian_diagnosis_ids
            .iter()
            .copied()
            .take(16)
            .collect(),
        rollback: entry.rollback,
        fields: operation_journal_proof_fields(entry),
        retention: RetentionClass::Proof,
    }
}

fn sanitized_operation_id(value: &str) -> OperationId {
    OperationId::new(
        sanitize_proof_identifier(value, 128).unwrap_or_else(|| "operation".to_string()),
    )
}

fn sanitized_target_descriptor(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        sanitize_proof_identifier(&target.id, 96).unwrap_or_else(|| "target".to_string()),
        target.ownership,
    )
}

fn operation_journal_proof_fields(entry: &OperationJournalEntry) -> Vec<EvidenceField> {
    let mut fields = Vec::new();
    push_token_field(
        &mut fields,
        "journal_id",
        entry.journal_id.as_str(),
        128,
        "journal",
    );
    push_token_field(
        &mut fields,
        "owner",
        format!("{:?}", entry.owner),
        48,
        "owner",
    );
    push_token_field(
        &mut fields,
        "ownership",
        format!("{:?}", entry.ownership),
        48,
        "ownership",
    );
    push_token_field(
        &mut fields,
        "planned_step_count",
        entry.planned_steps.len().to_string(),
        16,
        "0",
    );
    push_token_field(
        &mut fields,
        "completed_step_count",
        entry.completed_steps.len().to_string(),
        16,
        "0",
    );

    if let Some(step) = entry.completed_steps.last() {
        push_token_field(&mut fields, "latest_step_id", &step.step_id, 96, "step");
        push_token_field(
            &mut fields,
            "latest_step_phase",
            format!("{:?}", step.phase),
            48,
            "phase",
        );
        push_token_field(
            &mut fields,
            "latest_step_result",
            format!("{:?}", step.result),
            48,
            "result",
        );
        push_token_field(
            &mut fields,
            "latest_step_rollback",
            format!("{:?}", step.rollback),
            48,
            "rollback",
        );
        if let Some(target) = &step.changed_target {
            push_token_field(
                &mut fields,
                "latest_changed_target",
                &target.id,
                96,
                "target",
            );
        }
        for fact in step.generated_facts.iter().take(12) {
            push_text_field(&mut fields, "generated_fact", fact, 240, "redacted");
        }
    }

    fields
}

fn push_token_field(
    fields: &mut Vec<EvidenceField>,
    key: impl Into<String>,
    value: impl AsRef<str>,
    max_chars: usize,
    fallback: &str,
) {
    let value = sanitize_proof_identifier(value.as_ref(), max_chars)
        .unwrap_or_else(|| fallback.to_string());
    fields.push(EvidenceField::new(key, value, EvidenceSensitivity::Public));
}

fn push_text_field(
    fields: &mut Vec<EvidenceField>,
    key: impl Into<String>,
    value: impl AsRef<str>,
    max_chars: usize,
    fallback: &str,
) {
    let value = sanitize_evidence_text(
        value.as_ref(),
        RedactionAudience::ExportableProof,
        max_chars,
    )
    .unwrap_or_else(|| fallback.to_string());
    fields.push(EvidenceField::new(key, value, EvidenceSensitivity::Public));
}

fn sanitize_proof_identifier(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.chars().count() > max_chars
        || proof_identifier_looks_sensitive(value)
        || !value.chars().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '+' | ':')
        })
    {
        return None;
    }

    Some(value.to_string())
}

fn proof_identifier_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.contains('/') || value.contains('\\') || contains_windows_drive_path(value) {
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
        || lower.contains("--")
        || lower.contains("--classpath")
        || lower.contains("-classpath")
    {
        return true;
    }
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("provider_payload")
        || lower.contains("account_id")
        || lower.contains("username")
        || lower.contains("xuid")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("bearer")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }

    looks_like_jwt(value)
        || looks_like_bare_uuid_or_hex_id(value)
        || has_secret_like_identifier_segment(value)
}

fn looks_like_bare_uuid_or_hex_id(value: &str) -> bool {
    let compact = value.replace('-', "");
    compact.len() == 32 && compact.chars().all(|value| value.is_ascii_hexdigit())
}

fn has_secret_like_identifier_segment(value: &str) -> bool {
    value
        .split(|value: char| !(value.is_ascii_alphanumeric()))
        .any(|part| {
            part.len() >= 48
                && part.chars().any(|value| value.is_ascii_alphabetic())
                && part.chars().any(|value| value.is_ascii_digit())
        })
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

pub fn sanitize_public_json_value(
    value: serde_json::Value,
    audience: RedactionAudience,
    max_text_chars: usize,
    max_token_chars: usize,
) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Some(value)
        }
        serde_json::Value::String(value) => {
            sanitize_evidence_text(&value, audience, max_text_chars).map(serde_json::Value::String)
        }
        serde_json::Value::Array(values) => {
            let values = values
                .into_iter()
                .filter_map(|value| {
                    sanitize_public_json_value(value, audience, max_text_chars, max_token_chars)
                })
                .collect::<Vec<_>>();
            Some(serde_json::Value::Array(values))
        }
        serde_json::Value::Object(values) => {
            let mut sanitized = serde_json::Map::new();
            for (key, value) in values {
                let Some(key) = sanitize_evidence_token(&key, audience, max_token_chars) else {
                    continue;
                };
                if let Some(value) =
                    sanitize_public_json_value(value, audience, max_text_chars, max_token_chars)
                {
                    sanitized.insert(key, value);
                }
            }
            Some(serde_json::Value::Object(sanitized))
        }
    }
}

pub fn sanitize_public_log_line(
    value: &str,
    audience: RedactionAudience,
    max_chars: usize,
) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty()
        || value.chars().any(char::is_control)
        || !audience.allows(EvidenceSensitivity::Public)
    {
        return PUBLIC_LOG_LINE_REDACTED.to_string();
    }

    let value = if value.chars().count() > max_chars {
        truncate_chars(&value, max_chars)
    } else {
        value
    };

    if log_text_looks_sensitive(&value) {
        PUBLIC_LOG_LINE_REDACTED.to_string()
    } else {
        value
    }
}

pub fn sanitize_public_log_text(
    value: &str,
    audience: RedactionAudience,
    max_line_chars: usize,
) -> String {
    value
        .lines()
        .map(|line| sanitize_public_log_line(line, audience, max_line_chars))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn sanitize_public_diagnostic_text(
    value: &str,
    audience: RedactionAudience,
    max_chars: usize,
    fallback: &str,
) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty()
        || !audience.allows(EvidenceSensitivity::Public)
        || diagnostic_text_looks_sensitive(&value)
    {
        return fallback.to_string();
    }

    let sanitized = value
        .chars()
        .filter(|value| !value.is_control() && *value != ';')
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

pub(crate) fn bounded_descriptor_token(value: &str, fallback_prefix: &str) -> String {
    let value = value.trim();
    let safe = !value.is_empty()
        && value.len() <= 96
        && value
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'));
    if safe {
        return value.to_string();
    }

    format!("{fallback_prefix}-{:016x}", stable_descriptor_hash(value))
}

fn stable_descriptor_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
        || lower.contains("setting user:")
        || lower.contains("uuid of player")
        || lower.contains("--")
        || lower.contains("-x")
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
        || lower.contains("authorization")
        || lower.contains("credential")
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

pub fn log_text_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("appdata")
        || lower.contains("/home/")
        || lower.contains("/users/")
        || lower.contains("/var/")
        || lower.contains("/tmp/")
        || lower.contains("/opt/")
        || lower.contains("/usr/")
        || lower.contains("/etc/")
        || lower.contains("/library/")
        || lower.contains("/applications/")
        || lower.contains("/mnt/")
        || lower.contains("/volumes/")
        || lower.contains("~/")
        || lower.contains("\\users\\")
        || lower.contains("\\appdata\\")
        || contains_windows_drive_path(value)
    {
        return true;
    }
    if lower.contains(".minecraft")
        || lower.contains(".jar")
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
        || lower.contains("--")
        || lower.contains("-x")
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
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("bearer ")
    {
        return true;
    }
    if value.contains('@') && value.contains('.') {
        return true;
    }

    looks_like_jwt(value) || has_long_secret_like_run(value)
}

pub fn diagnostic_text_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    evidence_text_looks_sensitive(value)
        || lower.contains("account")
        || lower.contains("provider")
        || lower.contains("username")
        || lower.contains("uuid")
        || lower.contains("sessionid")
        || lower.contains("clientid")
        || lower.contains("java path")
        || lower.contains("java_path")
        || lower.contains("jvm")
        || lower.contains("args")
        || lower.contains("${")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    value.chars().take(max_chars).collect()
}

fn contains_windows_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'\\' | b'/')
    })
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
        RetentionClass, operation_journal_proof_record, performance_health_proof_record,
        sanitize_evidence_text, sanitize_evidence_token, sanitize_public_diagnostic_text,
        sanitize_public_json_value, sanitize_public_log_line,
    };
    use crate::guardian::DiagnosisId;
    use crate::state::contracts::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OperationStepResult, OwnershipClass,
        RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
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
    fn user_visible_json_sanitizer_filters_sensitive_nested_values() {
        let value = serde_json::json!({
            "decision": "blocked",
            "message": "Guardian applied managed defaults.",
            "details": [
                "safe fallback applied",
                "java path C:\\Users\\Alice\\AppData\\Local\\java.exe",
                "-Xmx8192M"
            ],
            "provider_payload": { "token": "secret" },
            "account_id": "abc"
        });

        let sanitized = sanitize_public_json_value(value, RedactionAudience::UserVisible, 180, 64)
            .expect("sanitize json");
        let encoded = serde_json::to_string(&sanitized).expect("serialize json");

        assert!(encoded.contains("Guardian applied managed defaults."));
        assert!(encoded.contains("safe fallback applied"));
        for fragment in [
            "Alice",
            "AppData",
            "java.exe",
            "-Xmx8192M",
            "provider_payload",
            "secret",
            "account_id",
        ] {
            assert!(
                !encoded.contains(fragment),
                "public json leaked fragment {fragment:?}: {encoded}"
            );
        }
    }

    #[test]
    fn public_log_line_sanitizer_allows_minecraft_thread_markers_but_redacts_private_lines() {
        let visible = sanitize_public_log_line(
            "[Render thread/INFO]: Reloading ResourceManager: vanilla",
            RedactionAudience::UserVisible,
            240,
        );
        let hidden = sanitize_public_log_line(
            "[main/ERROR]: failed for /home/alice/.minecraft java.exe --accessToken raw-secret -Xmx8192M",
            RedactionAudience::UserVisible,
            240,
        );

        assert_eq!(
            visible,
            "[Render thread/INFO]: Reloading ResourceManager: vanilla"
        );
        assert_eq!(hidden, super::PUBLIC_LOG_LINE_REDACTED);
    }

    #[test]
    fn public_diagnostic_sanitizer_bounds_safe_text_and_uses_domain_fallback_for_sensitive_text() {
        let safe = sanitize_public_diagnostic_text(
            &format!("safe diagnostic; {}", "x".repeat(220)),
            RedactionAudience::UserVisible,
            64,
            "fallback",
        );
        assert!(safe.len() <= 64);
        assert!(!safe.contains(';'));
        assert!(safe.starts_with("safe diagnostic"));

        for sensitive in [
            "/home/alice/.minecraft",
            r"C:\Users\Alice\AppData\java.exe",
            "command failed with --jvm-args -Xmx8192M",
            "provider returned {\"token\":\"secret\"}",
            "account_id=abc username=SecretPlayer",
            "Authorization: Bearer raw-token",
        ] {
            assert_eq!(
                sanitize_public_diagnostic_text(
                    sensitive,
                    RedactionAudience::UserVisible,
                    160,
                    "fallback"
                ),
                "fallback",
                "sensitive diagnostic survived: {sensitive}"
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

    #[test]
    fn operation_journal_proof_connects_redacted_facts_and_guardian_outcome() {
        let mut entry = OperationJournalEntry::new(
            JournalId::new("journal-install-operation-1"),
            OperationId::new("install-operation-1"),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::Unavailable,
        );
        entry.status = OperationStatus::Failed;
        entry.outcome = Some(OperationOutcome::Failed);
        entry.failure_point = Some("install_worker_interrupted".to_string());
        entry.targets.push(TargetDescriptor {
            system: StabilizationSystem::Application,
            kind: TargetKind::FilesystemPath,
            id: r"C:\Users\Alice\.minecraft\versions\secret.jar".to_string(),
            ownership: OwnershipClass::LauncherManaged,
        });
        entry
            .guardian_diagnosis_ids
            .push(DiagnosisId::DownloadUnavailable);
        let mut step = OperationJournalStep::new("install_progress_error", OperationPhase::Failed);
        step.result = OperationStepResult::Failed;
        step.generated_facts
            .push("guardian_outcome_decision:retry".to_string());
        step.generated_facts.push(
            "guardian_outcome_summary:Guardian treated install download failure as retryable."
                .to_string(),
        );
        step.generated_facts
            .push(r"C:\Users\Alice\.minecraft --accessToken secret -Xmx8192M".to_string());
        entry.completed_steps.push(step);

        let proof = operation_journal_proof_record(&entry);
        let encoded = serde_json::to_string(&proof).expect("serialize operation proof");

        assert_eq!(proof.operation_id, OperationId::new("install-operation-1"));
        assert_eq!(proof.status, OperationStatus::Failed);
        assert_eq!(proof.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            proof.failure_point.as_deref(),
            Some("install_worker_interrupted")
        );
        assert_eq!(
            proof.guardian_diagnosis_ids,
            vec![DiagnosisId::DownloadUnavailable]
        );
        assert!(encoded.contains(r#""guardian_diagnosis_ids":["download_unavailable"]"#));
        assert!(proof.fields.iter().any(|field| {
            field.key == "generated_fact" && field.value == "guardian_outcome_decision:retry"
        }));
        assert!(proof.fields.iter().any(|field| {
            field.key == "generated_fact" && field.value.contains("Guardian treated install")
        }));
        assert!(
            proof
                .fields
                .iter()
                .any(|field| { field.key == "generated_fact" && field.value == "redacted" })
        );
        assert!(encoded.contains("\"retention\":\"Proof\""));
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("secret.jar"));
        assert!(!encoded.contains("accessToken"));
        assert!(!encoded.contains("-Xmx"));
    }

    #[test]
    fn operation_journal_proof_preserves_structured_ids_without_leaking_bare_ids() {
        let operation_id = "install-operation-install-123e4567-e89b-12d3-a456-426614174000";
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{operation_id}")),
            OperationId::new(operation_id),
            CommandKind::InstallVersion,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::Unavailable,
        );
        entry.status = OperationStatus::Failed;
        entry.outcome = Some(OperationOutcome::Failed);
        entry.failure_point = Some("install_progress_error".to_string());
        entry
            .guardian_diagnosis_ids
            .push(DiagnosisId::DownloadUnavailable);
        entry.targets.push(TargetDescriptor::new(
            StabilizationSystem::Application,
            TargetKind::Version,
            "version-1.20.1",
            OwnershipClass::LauncherManaged,
        ));

        let proof = operation_journal_proof_record(&entry);

        assert_eq!(proof.operation_id, OperationId::new(operation_id));
        assert_eq!(
            proof.guardian_diagnosis_ids,
            vec![DiagnosisId::DownloadUnavailable]
        );
        assert!(proof.fields.iter().any(|field| {
            field.key == "journal_id" && field.value == format!("journal-{operation_id}")
        }));
    }
}
