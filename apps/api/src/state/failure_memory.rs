//! Guardian failure-memory state contracts.
//!
//! This module owns the bounded records Guardian consumes for loop suppression
//! and later-operation guidance. It stores memory; it does not decide policy.

use super::contracts::{OwnershipClass, TargetDescriptor, sanitize_target_id};
use super::ownership::{CurrentArtifact, classify_current_artifact};
use crate::execution::persistence::{
    AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease, WriteUrgency,
};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
use axial_config::AppPaths;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tracing::warn;

pub const FAILURE_MEMORY_SCHEMA: &str = "axial.guardian.failure_memory.v1";
pub const DEFAULT_FAILURE_MEMORY_LIMIT: usize = 64;
const FAILURE_MEMORY_FILE: &str = "failure-memory.json";
const FAILURE_MEMORY_LOCK_INVARIANT: &str =
    "Guardian failure-memory records lock poisoned; in-memory and persisted state may diverge";

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

#[derive(Debug, thiserror::Error)]
pub enum FailureMemoryStoreError {
    #[error("invalid Guardian failure-memory entry: {0:?}")]
    Validation(FailureMemoryValidationError),
    #[error("invalid Guardian failure-memory snapshot: {0:?}")]
    Snapshot(FailureMemoryLoadError),
    #[error("Guardian failure-memory persistence failed: {0}")]
    Persistence(#[source] io::Error),
}

impl FailureMemoryStoreError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Snapshot(_) => "snapshot",
            Self::Persistence(_) => "persistence",
        }
    }
}

impl From<FailureMemoryValidationError> for FailureMemoryStoreError {
    fn from(error: FailureMemoryValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<FailureMemoryLoadError> for FailureMemoryStoreError {
    fn from(error: FailureMemoryLoadError) -> Self {
        Self::Snapshot(error)
    }
}

struct FailureMemoryPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl FailureMemoryPersistence {
    fn claim(
        storage_path: &Path,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, FailureMemoryStoreError> {
        let owner = coordinator
            .claim_owner(storage_path)
            .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        let writer = owner
            .writer(storage_path, failure_memory_target())
            .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        Ok(Self { owner, writer })
    }
}

pub struct GuardianFailureMemoryStore {
    records: RwLock<BTreeMap<String, GuardianFailureMemoryEntry>>,
    max_entries: usize,
    persistence: Option<FailureMemoryPersistence>,
}

impl GuardianFailureMemoryStore {
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_FAILURE_MEMORY_LIMIT)
    }

    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            records: RwLock::new(BTreeMap::new()),
            max_entries: max_entries.clamp(1, DEFAULT_FAILURE_MEMORY_LIMIT),
            persistence: None,
        }
    }

    pub fn load_from_paths(paths: &AppPaths) -> Self {
        Self::try_load_from_paths(paths).unwrap_or_else(|error| {
            panic!("failed to initialize Guardian failure-memory persistence: {error}")
        })
    }

    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, FailureMemoryStoreError> {
        Self::try_load_from_paths_with_coordinator(paths, PersistenceCoordinator::global())
    }

    fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, FailureMemoryStoreError> {
        let storage_path = failure_memory_path(paths);
        let store = Self::with_max_entries_and_persistence(
            DEFAULT_FAILURE_MEMORY_LIMIT,
            Some(FailureMemoryPersistence::claim(&storage_path, coordinator)?),
        );

        match fs::read_to_string(&storage_path) {
            Ok(data) => match FailureMemorySnapshot::from_json(&data) {
                Ok(snapshot) => {
                    if let Err(error) = store.load_snapshot(snapshot) {
                        warn!(
                            error = ?error,
                            "failed to load persisted Guardian failure memory snapshot"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        "failed to parse persisted Guardian failure memory snapshot"
                    );
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    error = %error,
                    "failed to read persisted Guardian failure memory snapshot"
                );
            }
        }

        Ok(store)
    }

    fn with_max_entries_and_persistence(
        max_entries: usize,
        persistence: Option<FailureMemoryPersistence>,
    ) -> Self {
        Self {
            records: RwLock::new(BTreeMap::new()),
            max_entries: max_entries.clamp(1, DEFAULT_FAILURE_MEMORY_LIMIT),
            persistence,
        }
    }

    /// Accepts the updated snapshot for persistence before publishing it in memory.
    ///
    /// Success means the revision is owned by the persistence coordinator. Call
    /// [`Self::flush`] when the physical write must be observed before continuing.
    pub fn record(&self, entry: GuardianFailureMemoryEntry) -> Result<(), FailureMemoryStoreError> {
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        entry.validate()?;
        let mut candidate = records.clone();
        apply_record(&mut candidate, entry);
        prune_records(&mut candidate, self.max_entries);
        let snapshot = FailureMemorySnapshot::new(candidate.values().cloned().collect())?;
        if let Some(persistence) = &self.persistence {
            persistence
                .writer
                .accept(snapshot, WriteUrgency::Debounced, encode_snapshot)
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        }
        *records = candidate;
        Ok(())
    }

    pub fn get(&self, key: &FailureMemoryKey) -> Option<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .get(key.as_str())
            .cloned()
    }

    pub fn list(&self) -> Vec<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .values()
            .cloned()
            .collect()
    }

    pub fn snapshot(&self) -> Result<FailureMemorySnapshot, FailureMemoryLoadError> {
        FailureMemorySnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: FailureMemorySnapshot,
    ) -> Result<(), FailureMemoryLoadError> {
        snapshot.validate()?;
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        records.clear();
        for entry in snapshot.entries {
            records.insert(entry.key.as_str().to_string(), entry);
        }
        prune_records(&mut records, self.max_entries);
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), FailureMemoryStoreError> {
        if let Some(persistence) = &self.persistence {
            persistence
                .writer
                .flush()
                .await
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        }
        Ok(())
    }

    pub async fn retry(&self) -> Result<(), FailureMemoryStoreError> {
        if let Some(persistence) = &self.persistence {
            persistence
                .writer
                .retry()
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?
                .persisted()
                .await
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<(), FailureMemoryStoreError> {
        if let Some(persistence) = &self.persistence {
            persistence
                .writer
                .settle()
                .await
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
            persistence
                .owner
                .close()
                .await
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
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

fn apply_record(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    entry: GuardianFailureMemoryEntry,
) {
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

pub fn failure_memory_path(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("guardian").join(FAILURE_MEMORY_FILE)
}

fn failure_memory_target() -> TargetDescriptor {
    classify_current_artifact(
        CurrentArtifact::GuardianFailureMemorySnapshot,
        "guardian_failure_memory",
    )
    .target
}

fn encode_snapshot(snapshot: FailureMemorySnapshot) -> io::Result<Vec<u8>> {
    snapshot
        .to_json()
        .map(String::into_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::{
        FailureMemoryActionOutcome, FailureMemorySafeFallback, FailureMemorySafeFallbackKind,
        FailureMemorySnapshot, FailureMemoryStoreError, FailureMemoryUserDecision,
        FailureMemoryUserDecisionKind, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
    };
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
    use axial_config::AppPaths;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct CountingFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
    }

    impl CountingFileBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl AtomicWriteBackend for CountingFileBackend {
        fn write(
            &self,
            target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected failure-memory write failure"));
            }
            write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
                .map(|_| ())
                .map_err(io::Error::from)
        }
    }

    fn persistence_fixture(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<CountingFileBackend>,
        PersistenceCoordinator,
        GuardianFailureMemoryStore,
    ) {
        let root = test_root(name);
        let paths = test_paths(&root);
        let backend = Arc::new(CountingFileBackend::new());
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let store = GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(
            &paths,
            coordinator.clone(),
        )
        .expect("claim failure-memory persistence");
        (root, paths, backend, coordinator, store)
    }

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

    #[tokio::test]
    async fn failure_memory_burst_coalesces_and_reloads_the_latest_cumulative_snapshot() {
        let (root, paths, backend, _coordinator, store) =
            persistence_fixture("burst-latest-reload");
        let key = retry_entry("2026-06-15T10:00:00Z").key;

        for _ in 0..100 {
            store
                .record(retry_entry("2026-06-15T10:00:00Z"))
                .expect("record burst memory");
        }
        store.flush().await.expect("flush burst memory");

        assert!(backend.attempts.load(Ordering::SeqCst) < 10);
        let encoded = fs::read_to_string(super::failure_memory_path(&paths))
            .expect("read latest failure-memory snapshot");
        let snapshot = FailureMemorySnapshot::from_json(&encoded).expect("reload latest snapshot");
        let reloaded = GuardianFailureMemoryStore::new();
        reloaded
            .load_snapshot(snapshot)
            .expect("apply reloaded snapshot");
        assert_eq!(
            reloaded
                .get(&key)
                .expect("reloaded cumulative memory")
                .occurrence_count,
            100
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failure_memory_physical_failure_flushes_as_error_and_retries_latest_snapshot() {
        let (root, paths, backend, _coordinator, store) =
            persistence_fixture("physical-failure-retry");
        backend.fail_next();
        let key = retry_entry("2026-06-15T10:00:00Z").key;
        store
            .record(retry_entry("2026-06-15T10:00:00Z"))
            .expect("record first memory");
        store
            .record(retry_entry("2026-06-15T10:01:00Z"))
            .expect("record latest memory");

        assert!(matches!(
            store.flush().await,
            Err(FailureMemoryStoreError::Persistence(_))
        ));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        store.retry().await.expect("retry latest snapshot");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        let encoded = fs::read_to_string(super::failure_memory_path(&paths))
            .expect("read retried failure-memory snapshot");
        let snapshot = FailureMemorySnapshot::from_json(&encoded).expect("decode retried snapshot");
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].key, key);
        assert_eq!(snapshot.entries[0].occurrence_count, 2);
        assert_eq!(snapshot.entries[0].last_observed_at, "2026-06-15T10:01:00Z");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn close_retries_latest_failed_snapshot_and_releases_owner() {
        let (root, paths, backend, coordinator, store) =
            persistence_fixture("close-retries-latest");
        backend.fail_next();
        let key = retry_entry("2026-06-15T10:00:00Z").key;
        store
            .record(retry_entry("2026-06-15T10:00:00Z"))
            .expect("record first memory");
        store
            .record(retry_entry("2026-06-15T10:01:00Z"))
            .expect("record latest memory");

        store
            .close()
            .await
            .expect("close retries and settles latest snapshot");
        store.close().await.expect("close is idempotent");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        let reloaded =
            GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(&paths, coordinator)
                .expect("closed owner is released");
        let entry = reloaded.get(&key).expect("latest snapshot reloads");
        assert_eq!(entry.occurrence_count, 2);
        assert_eq!(entry.last_observed_at, "2026-06-15T10:01:00Z");
        reloaded.close().await.expect("close reloaded store");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failure_memory_snapshot_path_has_one_exclusive_owner() {
        let (root, paths, _backend, coordinator, first) = persistence_fixture("duplicate-owner");
        let second =
            GuardianFailureMemoryStore::try_load_from_paths_with_coordinator(&paths, coordinator);

        match second {
            Err(FailureMemoryStoreError::Persistence(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            }
            Err(error) => panic!("unexpected duplicate-owner error: {error}"),
            Ok(_) => panic!("duplicate failure-memory owner was accepted"),
        }

        first.close().await.expect("close first owner");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn successful_close_rejects_record_without_publishing_to_live_memory() {
        let (root, _paths, backend, _coordinator, store) = persistence_fixture("closed-acceptance");
        let entry = retry_entry("2026-06-15T10:00:00Z");
        let key = entry.key.clone();
        store.close().await.expect("close empty persistence");

        assert!(matches!(
            store.record(entry),
            Err(FailureMemoryStoreError::Persistence(_))
        ));
        assert!(store.get(&key).is_none());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn poisoned_failure_memory_lock_panics_on_every_access_before_accepting_a_write() {
        let (root, _paths, backend, _coordinator, store) = persistence_fixture("poisoned-lock");
        let entry = retry_entry("2026-06-15T10:00:00Z");
        let key = entry.key.clone();
        let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("valid snapshot");
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _records = store.records.write().expect("acquire records lock");
            panic!("inject failure-memory lock poison");
        }));
        assert!(poison.is_err());

        assert_lock_invariant_panic(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = store.record(entry);
            }))
            .expect_err("poisoned record must panic"),
        );
        assert_lock_invariant_panic(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.get(&key)))
                .expect_err("poisoned get must panic"),
        );
        assert_lock_invariant_panic(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store.list()))
                .expect_err("poisoned list must panic"),
        );
        assert_lock_invariant_panic(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = store.load_snapshot(snapshot);
            }))
            .expect_err("poisoned load must panic"),
        );
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failure_memory_store_persists_snapshot_for_restart_reasoning() {
        let root = test_root("persisted-snapshot");
        let paths = test_paths(&root);
        let store = GuardianFailureMemoryStore::load_from_paths(&paths);
        let entry =
            retry_entry("2026-06-15T10:00:00Z").with_suppression_until("2026-06-15T10:30:00Z");
        let key = entry.key.clone();

        store.record(entry).expect("record memory");
        store.flush().await.expect("flush failure memory");
        let path = super::failure_memory_path(&paths);
        assert!(path.is_file());

        store.close().await.expect("close failure memory");
        drop(store);
        let encoded = fs::read_to_string(&path).expect("read persisted failure memory");
        let snapshot = FailureMemorySnapshot::from_json(&encoded).expect("decode persisted memory");
        let reloaded = GuardianFailureMemoryStore::new();
        reloaded
            .load_snapshot(snapshot)
            .expect("reload persisted memory");
        let persisted = reloaded.get(&key).expect("persisted memory");
        assert_eq!(
            persisted.suppression_until.as_deref(),
            Some("2026-06-15T10:30:00Z")
        );
        assert!(
            !serde_json::to_string(&persisted)
                .expect("memory json")
                .contains("-Xmx")
        );

        let _ = fs::remove_dir_all(&root);
    }

    fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
        if let Some(message) = panic.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else {
            "non-string panic".to_string()
        }
    }

    fn assert_lock_invariant_panic(panic: Box<dyn std::any::Any + Send>) {
        assert!(panic_message(panic).contains(super::FAILURE_MEMORY_LOCK_INVARIANT));
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

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "axial-failure-memory-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create test root");
        root
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
