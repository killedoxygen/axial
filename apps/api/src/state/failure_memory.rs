//! Guardian failure-memory state contracts.
//!
//! This module defines the State-owned shape Guardian will later consume for
//! loop suppression. It records memory; it does not decide Guardian policy.

use super::contracts::{OwnershipClass, TargetDescriptor, sanitize_target_id};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::RwLock;

pub const FAILURE_MEMORY_SCHEMA: &str = "croopor.guardian.failure_memory.v1";
pub const DEFAULT_FAILURE_MEMORY_LIMIT: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct FailureMemoryKey(pub String);

impl FailureMemoryKey {
    pub fn for_observation(
        domain: GuardianDomain,
        diagnosis_id: &DiagnosisId,
        target: &TargetDescriptor,
        mode: GuardianMode,
        user_intent_hash: Option<&str>,
    ) -> Self {
        let diagnosis = sanitize_target_id(diagnosis_id.as_str(), "diagnosis");
        let target_id = sanitize_target_id(&target.id, "target");
        let intent = user_intent_hash
            .map(|value| sanitize_target_id(value, "intent"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "no_intent".to_string());
        Self(format!(
            "{domain:?}:{diagnosis}:{:?}.{:?}.{target_id}:{mode:?}:{intent}",
            target.system, target.kind
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardianFailureMemoryEntry {
    pub key: FailureMemoryKey,
    pub diagnosis_id: DiagnosisId,
    pub domain: GuardianDomain,
    pub mode: GuardianMode,
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub first_observed_at: String,
    pub last_observed_at: String,
    pub occurrence_count: u32,
    pub last_action_kind: Option<GuardianActionKind>,
    pub last_action_outcome: Option<FailureMemoryActionOutcome>,
    pub repair_attempt_count: u32,
    pub quarantined_target: Option<TargetDescriptor>,
    pub suppression_until: Option<String>,
    pub safe_fallback: Option<FailureMemorySafeFallback>,
    pub user_decision: Option<FailureMemoryUserDecision>,
    pub target_content_hash: Option<String>,
    pub user_intent_hash: Option<String>,
}

impl GuardianFailureMemoryEntry {
    pub fn observed(
        diagnosis_id: DiagnosisId,
        domain: GuardianDomain,
        target: TargetDescriptor,
        mode: GuardianMode,
        user_intent_hash: Option<&str>,
        observed_at: impl Into<String>,
    ) -> Self {
        let observed_at = observed_at.into();
        let diagnosis_id = DiagnosisId::new(sanitize_target_id(diagnosis_id.as_str(), "diagnosis"));
        let user_intent_hash = user_intent_hash
            .map(|value| sanitize_target_id(value, "intent"))
            .filter(|value| !value.is_empty());
        let key = FailureMemoryKey::for_observation(
            domain,
            &diagnosis_id,
            &target,
            mode,
            user_intent_hash.as_deref(),
        );
        Self {
            key,
            diagnosis_id,
            domain,
            mode,
            ownership: target.ownership,
            target,
            first_observed_at: observed_at.clone(),
            last_observed_at: observed_at,
            occurrence_count: 1,
            last_action_kind: None,
            last_action_outcome: None,
            repair_attempt_count: 0,
            quarantined_target: None,
            suppression_until: None,
            safe_fallback: None,
            user_decision: None,
            target_content_hash: None,
            user_intent_hash,
        }
    }

    pub fn with_action(
        mut self,
        action_kind: GuardianActionKind,
        outcome: FailureMemoryActionOutcome,
    ) -> Self {
        self.last_action_kind = Some(action_kind);
        self.last_action_outcome = Some(outcome);
        self
    }

    pub fn with_repair_attempt(mut self) -> Self {
        self.repair_attempt_count = self.repair_attempt_count.saturating_add(1);
        self
    }

    pub fn with_quarantined_target(mut self, target: TargetDescriptor) -> Self {
        self.quarantined_target = Some(target);
        self
    }

    pub fn with_suppression_until(mut self, suppression_until: impl Into<String>) -> Self {
        self.suppression_until = non_empty_string(suppression_until.into());
        self
    }

    pub fn with_safe_fallback(mut self, safe_fallback: FailureMemorySafeFallback) -> Self {
        self.safe_fallback = Some(safe_fallback);
        self
    }

    pub fn with_user_decision(mut self, user_decision: FailureMemoryUserDecision) -> Self {
        self.user_decision = Some(user_decision);
        self
    }

    pub fn with_target_content_hash(mut self, target_content_hash: impl AsRef<str>) -> Self {
        self.target_content_hash =
            safe_optional_fragment(target_content_hash.as_ref(), "target_hash");
        self
    }

    pub fn target_content_changed(&self, current_hash: &str) -> bool {
        safe_optional_fragment(current_hash, "target_hash") != self.target_content_hash
    }

    pub fn validate(&self) -> Result<(), FailureMemoryValidationError> {
        if !is_safe_memory_fragment(self.key.as_str()) {
            return Err(FailureMemoryValidationError::UnsafeKey);
        }
        if self.key
            != FailureMemoryKey::for_observation(
                self.domain,
                &self.diagnosis_id,
                &self.target,
                self.mode,
                self.user_intent_hash.as_deref(),
            )
        {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch);
        }
        if !is_safe_memory_fragment(self.diagnosis_id.as_str()) {
            return Err(FailureMemoryValidationError::UnsafeDiagnosisId);
        }
        if !is_safe_memory_fragment(&self.target.id) {
            return Err(FailureMemoryValidationError::UnsafeTargetId);
        }
        if self.ownership != self.target.ownership {
            return Err(FailureMemoryValidationError::OwnershipMismatch);
        }
        if self.occurrence_count == 0 {
            return Err(FailureMemoryValidationError::ZeroOccurrences);
        }
        let first_observed_at = parse_timestamp(&self.first_observed_at)
            .map_err(|_| FailureMemoryValidationError::InvalidObservedTimestamp)?;
        let last_observed_at = parse_timestamp(&self.last_observed_at)
            .map_err(|_| FailureMemoryValidationError::InvalidObservedTimestamp)?;
        if last_observed_at < first_observed_at {
            return Err(FailureMemoryValidationError::InvalidObservedTimestamp);
        }
        if let Some(target) = &self.quarantined_target
            && !is_safe_memory_fragment(&target.id)
        {
            return Err(FailureMemoryValidationError::UnsafeTargetId);
        }
        if let Some(suppression_until) = &self.suppression_until
            && parse_timestamp(suppression_until).is_err()
        {
            return Err(FailureMemoryValidationError::InvalidSuppressionTimestamp);
        }
        if let Some(safe_fallback) = &self.safe_fallback
            && !is_safe_memory_fragment(&safe_fallback.id)
        {
            return Err(FailureMemoryValidationError::UnsafeFallbackId);
        }
        if let Some(user_decision) = &self.user_decision {
            user_decision.validate()?;
        }
        if let Some(target_content_hash) = &self.target_content_hash
            && !is_safe_memory_fragment(target_content_hash)
        {
            return Err(FailureMemoryValidationError::UnsafeTargetHash);
        }
        if let Some(user_intent_hash) = &self.user_intent_hash
            && !is_safe_memory_fragment(user_intent_hash)
        {
            return Err(FailureMemoryValidationError::UnsafeUserIntentHash);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailureMemoryActionOutcome {
    NotNeeded,
    Repaired,
    Quarantined,
    RolledBack,
    Retried,
    Degraded,
    Blocked,
    Failed,
    Suppressed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureMemorySafeFallback {
    pub kind: FailureMemorySafeFallbackKind,
    pub id: String,
}

impl FailureMemorySafeFallback {
    pub fn new(kind: FailureMemorySafeFallbackKind, id: impl AsRef<str>) -> Self {
        Self {
            kind,
            id: sanitize_target_id(id.as_ref(), "fallback"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailureMemorySafeFallbackKind {
    ManagedRuntime,
    BuiltInPerformanceRules,
    PreviousPerformanceComposition,
    VanillaMode,
    UserGuidance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureMemoryUserDecision {
    pub decision: FailureMemoryUserDecisionKind,
    pub decided_at: String,
    pub suppression_until: Option<String>,
    pub input_hash: Option<String>,
}

impl FailureMemoryUserDecision {
    pub fn new(decision: FailureMemoryUserDecisionKind, decided_at: impl Into<String>) -> Self {
        Self {
            decision,
            decided_at: decided_at.into(),
            suppression_until: None,
            input_hash: None,
        }
    }

    pub fn with_suppression_until(mut self, suppression_until: impl Into<String>) -> Self {
        self.suppression_until = non_empty_string(suppression_until.into());
        self
    }

    pub fn with_input_hash(mut self, input_hash: impl AsRef<str>) -> Self {
        self.input_hash = safe_optional_fragment(input_hash.as_ref(), "input_hash");
        self
    }

    fn validate(&self) -> Result<(), FailureMemoryValidationError> {
        if parse_timestamp(&self.decided_at).is_err() {
            return Err(FailureMemoryValidationError::InvalidDecisionTimestamp);
        }
        if let Some(suppression_until) = &self.suppression_until
            && parse_timestamp(suppression_until).is_err()
        {
            return Err(FailureMemoryValidationError::InvalidSuppressionTimestamp);
        }
        if let Some(input_hash) = &self.input_hash
            && !is_safe_memory_fragment(input_hash)
        {
            return Err(FailureMemoryValidationError::UnsafeUserDecisionHash);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailureMemoryUserDecisionKind {
    Accepted,
    Declined,
    Deferred,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureMemorySnapshot {
    pub schema: String,
    pub entries: Vec<GuardianFailureMemoryEntry>,
}

impl FailureMemorySnapshot {
    pub fn new(entries: Vec<GuardianFailureMemoryEntry>) -> Result<Self, FailureMemoryLoadError> {
        let snapshot = Self {
            schema: FAILURE_MEMORY_SCHEMA.to_string(),
            entries,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn from_json(value: &str) -> Result<Self, FailureMemoryLoadError> {
        let snapshot = serde_json::from_str::<Self>(value)?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    fn validate(&self) -> Result<(), FailureMemoryLoadError> {
        if self.schema != FAILURE_MEMORY_SCHEMA {
            return Err(FailureMemoryLoadError::InvalidSchema);
        }
        if self.entries.len() > DEFAULT_FAILURE_MEMORY_LIMIT {
            return Err(FailureMemoryLoadError::TooManyEntries);
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum FailureMemoryLoadError {
    Json(serde_json::Error),
    InvalidSchema,
    TooManyEntries,
    InvalidEntry(FailureMemoryValidationError),
}

impl From<serde_json::Error> for FailureMemoryLoadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<FailureMemoryValidationError> for FailureMemoryLoadError {
    fn from(error: FailureMemoryValidationError) -> Self {
        Self::InvalidEntry(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureMemoryValidationError {
    UnsafeKey,
    UnsafeDiagnosisId,
    UnsafeTargetId,
    UnsafeFallbackId,
    UnsafeTargetHash,
    UnsafeUserIntentHash,
    UnsafeUserDecisionHash,
    MemoryKeyMismatch,
    OwnershipMismatch,
    ZeroOccurrences,
    InvalidObservedTimestamp,
    InvalidSuppressionTimestamp,
    InvalidDecisionTimestamp,
}

pub struct GuardianFailureMemoryStore {
    records: RwLock<BTreeMap<String, GuardianFailureMemoryEntry>>,
    max_entries: usize,
}

impl GuardianFailureMemoryStore {
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_FAILURE_MEMORY_LIMIT)
    }

    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            records: RwLock::new(BTreeMap::new()),
            max_entries: max_entries.max(1),
        }
    }

    pub fn record(
        &self,
        entry: GuardianFailureMemoryEntry,
    ) -> Result<(), FailureMemoryValidationError> {
        entry.validate()?;
        if let Ok(mut records) = self.records.write() {
            let key = entry.key.as_str().to_string();
            if let Some(existing) = records.get_mut(&key) {
                let first_observed_at = existing.first_observed_at.clone();
                let occurrence_count = existing
                    .occurrence_count
                    .saturating_add(entry.occurrence_count.max(1));
                let repair_attempt_count = existing
                    .repair_attempt_count
                    .saturating_add(entry.repair_attempt_count);
                *existing = entry;
                existing.first_observed_at = first_observed_at;
                existing.occurrence_count = occurrence_count;
                existing.repair_attempt_count = repair_attempt_count;
            } else {
                records.insert(key, entry);
            }
            prune_records(&mut records, self.max_entries);
        }
        Ok(())
    }

    pub fn get(&self, key: &FailureMemoryKey) -> Option<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .ok()
            .and_then(|records| records.get(key.as_str()).cloned())
    }

    pub fn list(&self) -> Vec<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .map(|records| records.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn snapshot(&self) -> Result<FailureMemorySnapshot, FailureMemoryLoadError> {
        FailureMemorySnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: FailureMemorySnapshot,
    ) -> Result<(), FailureMemoryLoadError> {
        snapshot.validate()?;
        if let Ok(mut records) = self.records.write() {
            records.clear();
            for entry in snapshot.entries {
                records.insert(entry.key.as_str().to_string(), entry);
            }
            prune_records(&mut records, self.max_entries);
        }
        Ok(())
    }
}

impl Default for GuardianFailureMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn prune_records(records: &mut BTreeMap<String, GuardianFailureMemoryEntry>, max_entries: usize) {
    if records.len() <= max_entries {
        return;
    }

    let mut ordered = records
        .values()
        .map(|entry| {
            (
                parse_timestamp(&entry.last_observed_at)
                    .map(|timestamp| timestamp.timestamp_millis())
                    .unwrap_or_default(),
                entry.key.as_str().to_string(),
            )
        })
        .collect::<Vec<_>>();
    ordered.sort();
    let remove_count = records.len().saturating_sub(max_entries);
    for (_, key) in ordered.into_iter().take(remove_count) {
        records.remove(&key);
    }
}

fn safe_optional_fragment(value: &str, fallback: &str) -> Option<String> {
    let value = sanitize_target_id(value, fallback);
    (!value.is_empty() && value != fallback).then_some(value)
}

fn non_empty_string(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn is_safe_memory_fragment(value: &str) -> bool {
    !value.trim().is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
}

fn parse_timestamp(value: &str) -> Result<DateTime<FixedOffset>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(value.trim())
}

#[cfg(test)]
mod tests {
    use super::{
        FailureMemoryActionOutcome, FailureMemorySafeFallback, FailureMemorySafeFallbackKind,
        FailureMemorySnapshot, FailureMemoryUserDecision, FailureMemoryUserDecisionKind,
        GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
    };
    use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};

    #[test]
    fn failure_memory_entry_round_trips_strict_shape() {
        let entry = retry_entry("2026-06-15T10:00:00Z")
            .with_suppression_until("2026-06-15T10:30:00Z")
            .with_safe_fallback(FailureMemorySafeFallback::new(
                FailureMemorySafeFallbackKind::ManagedRuntime,
                "managed_java_21",
            ))
            .with_target_content_hash("sha256abc123");
        let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("snapshot");
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = FailureMemorySnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![entry]);
    }

    #[test]
    fn failure_memory_rejects_unknown_fields_and_unsafe_target_ids() {
        let value = serde_json::json!({
            "schema": super::FAILURE_MEMORY_SCHEMA,
            "entries": [{
                "key": "Launch:bad_java_override:State.FilesystemPath.target:Managed:intent",
                "diagnosis_id": "bad_java_override",
                "domain": "Launch",
                "mode": "Managed",
                "target": {
                    "system": "State",
                    "kind": "FilesystemPath",
                    "id": "target",
                    "ownership": "UserOwned"
                },
                "ownership": "UserOwned",
                "first_observed_at": "2026-06-15T10:00:00Z",
                "last_observed_at": "2026-06-15T10:00:00Z",
                "occurrence_count": 1,
                "last_action_kind": "Retry",
                "last_action_outcome": "Failed",
                "repair_attempt_count": 0,
                "quarantined_target": null,
                "suppression_until": null,
                "safe_fallback": null,
                "user_decision": null,
                "target_content_hash": null,
                "user_intent_hash": "intent",
                "unexpected": true
            }]
        });

        assert!(FailureMemorySnapshot::from_json(&value.to_string()).is_err());

        let nested_unknown_field = serde_json::json!({
            "schema": super::FAILURE_MEMORY_SCHEMA,
            "entries": [{
                "key": "Launch:bad_java_override:State.FilesystemPath.target:Managed:intent",
                "diagnosis_id": "bad_java_override",
                "domain": "Launch",
                "mode": "Managed",
                "target": {
                    "system": "State",
                    "kind": "FilesystemPath",
                    "id": "target",
                    "ownership": "UserOwned",
                    "unexpected": true
                },
                "ownership": "UserOwned",
                "first_observed_at": "2026-06-15T10:00:00Z",
                "last_observed_at": "2026-06-15T10:00:00Z",
                "occurrence_count": 1,
                "last_action_kind": "Retry",
                "last_action_outcome": "Failed",
                "repair_attempt_count": 0,
                "quarantined_target": null,
                "suppression_until": null,
                "safe_fallback": null,
                "user_decision": null,
                "target_content_hash": null,
                "user_intent_hash": "intent"
            }]
        });
        assert!(FailureMemorySnapshot::from_json(&nested_unknown_field.to_string()).is_err());

        let unsafe_entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("bad_java_override"),
            GuardianDomain::Launch,
            TargetDescriptor {
                system: StabilizationSystem::State,
                kind: TargetKind::FilesystemPath,
                id: r"C:\Users\Alice\java.exe".to_string(),
                ownership: OwnershipClass::UserOwned,
            },
            GuardianMode::Managed,
            Some("intent"),
            "2026-06-15T10:00:00Z",
        );
        assert!(unsafe_entry.validate().is_err());

        let unsafe_fallback =
            retry_entry("2026-06-15T10:10:00Z").with_safe_fallback(FailureMemorySafeFallback {
                kind: FailureMemorySafeFallbackKind::ManagedRuntime,
                id: r"C:\Users\Alice\runtime".to_string(),
            });
        assert!(unsafe_fallback.validate().is_err());

        let unsafe_decision =
            retry_entry("2026-06-15T10:11:00Z").with_user_decision(FailureMemoryUserDecision {
                decision: FailureMemoryUserDecisionKind::Declined,
                decided_at: "2026-06-15T10:11:30Z".to_string(),
                suppression_until: Some("2026-06-15T10:30:00Z".to_string()),
                input_hash: Some("/home/alice/settings".to_string()),
            });
        assert!(unsafe_decision.validate().is_err());

        let bad_timestamp = retry_entry("not-a-date");
        assert!(bad_timestamp.validate().is_err());

        let mut mismatched_key = retry_entry("2026-06-15T10:12:00Z");
        mismatched_key.key.0 =
            "Launch:other:State.FilesystemPath.target:Managed:intent".to_string();
        assert_eq!(
            mismatched_key.validate(),
            Err(super::FailureMemoryValidationError::MemoryKeyMismatch)
        );
    }

    #[test]
    fn retry_and_repair_suppression_shape_records_attempts_without_policy() {
        let store = GuardianFailureMemoryStore::new();
        let retry =
            retry_entry("2026-06-15T10:00:00Z").with_suppression_until("2026-06-15T10:30:00Z");
        let retry_key = retry.key.clone();
        store.record(retry.clone()).expect("record retry");
        store.record(retry).expect("record repeated retry");
        let stored_retry = store.get(&retry_key).expect("stored retry");

        assert_eq!(stored_retry.occurrence_count, 2);
        assert_eq!(
            stored_retry.last_action_kind,
            Some(GuardianActionKind::Retry)
        );
        assert_eq!(
            stored_retry.last_action_outcome,
            Some(FailureMemoryActionOutcome::Failed)
        );
        assert_eq!(
            stored_retry.suppression_until.as_deref(),
            Some("2026-06-15T10:30:00Z")
        );

        let managed_artifact =
            classify_current_artifact(CurrentArtifact::ManagedRuntimeCache, "java_runtime_21")
                .target;
        let repair = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("managed_runtime_ready_marker_missing"),
            GuardianDomain::Runtime,
            managed_artifact.clone(),
            GuardianMode::Managed,
            Some("runtime_hash"),
            "2026-06-15T10:05:00Z",
        )
        .with_action(
            GuardianActionKind::Repair,
            FailureMemoryActionOutcome::Failed,
        )
        .with_repair_attempt()
        .with_quarantined_target(managed_artifact)
        .with_suppression_until("2026-06-15T10:20:00Z");

        assert_eq!(repair.repair_attempt_count, 1);
        assert!(repair.quarantined_target.is_some());
        assert_eq!(repair.ownership, OwnershipClass::LauncherManaged);
        assert!(repair.validate().is_ok());

        let repair_key = repair.key.clone();
        store.record(repair.clone()).expect("record repair");
        store.record(repair).expect("record repeated repair");
        let stored_repair = store.get(&repair_key).expect("stored repair");
        assert_eq!(stored_repair.occurrence_count, 2);
        assert_eq!(stored_repair.repair_attempt_count, 2);
    }

    #[test]
    fn user_decline_suppression_shape_records_decision() {
        let entry = retry_entry("2026-06-15T11:00:00Z")
            .with_action(
                GuardianActionKind::AskUser,
                FailureMemoryActionOutcome::Suppressed,
            )
            .with_user_decision(
                FailureMemoryUserDecision::new(
                    FailureMemoryUserDecisionKind::Declined,
                    "2026-06-15T11:00:30Z",
                )
                .with_suppression_until("2026-06-15T12:00:00Z")
                .with_input_hash("settings_hash_1"),
            );

        let decision = entry.user_decision.expect("user decision");
        assert_eq!(decision.decision, FailureMemoryUserDecisionKind::Declined);
        assert_eq!(
            decision.suppression_until.as_deref(),
            Some("2026-06-15T12:00:00Z")
        );
        assert_eq!(decision.input_hash.as_deref(), Some("settings_hash_1"));
    }

    #[test]
    fn changed_target_hash_reset_shape_is_explicit() {
        let entry = retry_entry("2026-06-15T12:00:00Z").with_target_content_hash("sha256_old123");

        assert!(!entry.target_content_changed("sha256_old123"));
        assert!(entry.target_content_changed("sha256_new456"));
    }

    #[test]
    fn invalid_remote_rules_suppression_shape_uses_external_provider_target() {
        let target = classify_current_artifact(
            CurrentArtifact::ExternalPerformanceRules,
            "performance_rules_remote_source",
        )
        .target;
        let entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("remote_rules_signature_invalid"),
            GuardianDomain::Performance,
            target,
            GuardianMode::Managed,
            Some("rules_manifest_v1"),
            "2026-06-15T13:00:00Z",
        )
        .with_action(
            GuardianActionKind::RecordOnly,
            FailureMemoryActionOutcome::Suppressed,
        )
        .with_safe_fallback(FailureMemorySafeFallback::new(
            FailureMemorySafeFallbackKind::BuiltInPerformanceRules,
            "builtin_rules",
        ))
        .with_suppression_until("2026-06-15T13:05:00Z");

        assert_eq!(entry.ownership, OwnershipClass::ExternalProviderDerived);
        assert_eq!(
            entry.safe_fallback.as_ref().map(|fallback| fallback.kind),
            Some(FailureMemorySafeFallbackKind::BuiltInPerformanceRules)
        );
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn failure_memory_store_bounds_retention_to_recent_entries() {
        let store = GuardianFailureMemoryStore::with_max_entries(2);
        for (diagnosis, observed_at) in [
            ("first_failure", "2026-06-15T10:00:00Z"),
            ("second_failure", "2026-06-15T10:01:00Z"),
            ("third_failure", "2026-06-15T10:02:00Z"),
        ] {
            let entry = GuardianFailureMemoryEntry::observed(
                DiagnosisId::new(diagnosis),
                GuardianDomain::Launch,
                classify_current_artifact(CurrentArtifact::UnknownFilesystemPath, diagnosis).target,
                GuardianMode::Managed,
                Some("intent"),
                observed_at,
            );
            store.record(entry).expect("record memory");
        }

        let entries = store.list();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.diagnosis_id.as_str() != "first_failure")
        );
    }

    fn retry_entry(observed_at: &str) -> GuardianFailureMemoryEntry {
        GuardianFailureMemoryEntry::observed(
            DiagnosisId::new("process_exited_before_boot_marker"),
            GuardianDomain::Launch,
            classify_current_artifact(CurrentArtifact::UserJvmArguments, "-Xmx16384M").target,
            GuardianMode::Managed,
            Some("intent_hash_1"),
            observed_at,
        )
        .with_action(
            GuardianActionKind::Retry,
            FailureMemoryActionOutcome::Failed,
        )
    }
}
