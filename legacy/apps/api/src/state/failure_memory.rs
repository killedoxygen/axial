//! Guardian failure-memory state contracts.
//!
//! This module owns the bounded records Guardian consumes for loop suppression
//! and later-operation guidance. It stores memory; it does not decide policy.

use super::contracts::{
    OwnershipClass, PersistedStateRepairAttempt, PersistedStateRepairTerminal,
    PersistedStateRepairTerminalOutcome, RECONCILIATION_EVIDENCE_CAPACITY, TargetDescriptor,
    sanitize_target_id,
};
use super::contracts::{
    ReconciliationComponent, ReconciliationQuarantineCheckpoint, ReconciliationRung,
    ReconciliationScope, ReconciliationTerminal, ReconciliationTerminalOutcome,
};
use super::successors::FAILURE_MEMORY_SNAPSHOT_SUCCESSOR;
use super::temporal::{
    BoundedTemporalDisposition, BoundedTemporalLoadIssueCounts, BoundedTemporalPolicy,
    BoundedTemporalRecord, BoundedTemporalViolation,
};
use crate::execution::anchored_record::AnchoredRecordDirectory;
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
#[cfg(test)]
use axial_config::AppPaths;
use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

pub const FAILURE_MEMORY_SCHEMA: &str = "axial.guardian.failure_memory.v7";
pub const DEFAULT_FAILURE_MEMORY_LIMIT: usize = RECONCILIATION_EVIDENCE_CAPACITY;
// The outer read bound follows the v7 record budget and fixed 128-entry capacity.
const MAX_FAILURE_MEMORY_ENTRY_BYTES: u64 = 16 * 1024;
const FAILURE_MEMORY_SNAPSHOT_FIXED_BYTES: u64 =
    (r#"{"schema":"","entries":[]}"#.len() + FAILURE_MEMORY_SCHEMA.len()) as u64;
const MAX_FAILURE_MEMORY_SNAPSHOT_BYTES: u64 = FAILURE_MEMORY_SNAPSHOT_FIXED_BYTES
    + DEFAULT_FAILURE_MEMORY_LIMIT as u64 * MAX_FAILURE_MEMORY_ENTRY_BYTES
    + (DEFAULT_FAILURE_MEMORY_LIMIT - 1) as u64;
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
        let diagnosis = diagnosis_id.as_str();
        let target_id = sanitize_target_id(&target.id, "target");
        let intent = user_intent_hash
            .map(|value| sanitize_target_id(value, "intent"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "no_intent".to_string());
        Self(format!(
            "{}:{diagnosis}:{}.{}.{target_id}:{}:{intent}",
            domain.failure_memory_id(),
            target.system.failure_memory_id(),
            target.kind.failure_memory_id(),
            mode.failure_memory_id(),
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn for_reconciliation(
        domain: GuardianDomain,
        diagnosis_id: &DiagnosisId,
        target: &TargetDescriptor,
        terminal: &ReconciliationTerminal,
    ) -> Self {
        Self::for_reconciliation_parts(
            domain,
            diagnosis_id,
            target,
            terminal.mode(),
            terminal.rung(),
            terminal.component(),
            terminal.scope(),
        )
    }

    pub(super) fn for_reconciliation_parts(
        domain: GuardianDomain,
        diagnosis_id: &DiagnosisId,
        target: &TargetDescriptor,
        mode: GuardianMode,
        rung: ReconciliationRung,
        component: ReconciliationComponent,
        reconciliation_scope: &ReconciliationScope,
    ) -> Self {
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
            activation_contract_id,
        } = reconciliation_scope;
        let scope = format!(
            "registered.{instance_id}.{}.{}.{}",
            fingerprint.as_str(),
            inventory_fingerprint.as_str(),
            activation_contract_id
        );
        let base = Self::for_observation(domain, diagnosis_id, target, mode, None);
        Self(format!(
            "{}:rung.{}:component.{}:scope.{scope}",
            base.as_str(),
            rung.failure_memory_id(),
            component.failure_memory_id(),
        ))
    }

    pub(super) fn for_persisted_state_repair(attempt: &PersistedStateRepairAttempt) -> Self {
        Self(format!(
            "State:{}:Managed:{}:{}:{}",
            DiagnosisId::PersistedStateSchemaInvalid.as_str(),
            attempt.store().failure_memory_id(),
            attempt.record_id(),
            attempt.physical_identity().as_str(),
        ))
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
    pub quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    pub suppression_until: Option<String>,
    pub target_content_hash: Option<String>,
    pub user_intent_hash: Option<String>,
    reconciliation_terminal: Option<ReconciliationTerminal>,
    persisted_state_repair_terminal: Option<PersistedStateRepairTerminal>,
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
            quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
            suppression_until: None,
            target_content_hash: None,
            user_intent_hash,
            reconciliation_terminal: None,
            persisted_state_repair_terminal: None,
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

    pub(super) fn with_quarantine_checkpoint(
        mut self,
        checkpoint: ReconciliationQuarantineCheckpoint,
    ) -> Self {
        self.quarantine_checkpoint = checkpoint;
        self
    }

    pub fn with_suppression_until(mut self, suppression_until: impl Into<String>) -> Self {
        self.suppression_until = non_empty_string(suppression_until.into());
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

    pub fn reconciliation_terminal(&self) -> Option<&ReconciliationTerminal> {
        self.reconciliation_terminal.as_ref()
    }

    pub(crate) fn persisted_state_repair_terminal(&self) -> Option<&PersistedStateRepairTerminal> {
        self.persisted_state_repair_terminal.as_ref()
    }

    fn has_pending_publication(&self) -> bool {
        self.reconciliation_terminal()
            .and_then(ReconciliationTerminal::version_bundle_publication)
            .is_some_and(|publication| publication.is_pending())
    }

    pub(super) fn with_reconciliation_terminal(mut self, terminal: ReconciliationTerminal) -> Self {
        self.mode = terminal.mode();
        self.ownership = terminal.ownership();
        self.target.ownership = terminal.ownership();
        self.user_intent_hash = None;
        self.key = FailureMemoryKey::for_reconciliation(
            self.domain,
            &self.diagnosis_id,
            &self.target,
            &terminal,
        );
        self.reconciliation_terminal = Some(terminal);
        self
    }

    pub(super) fn for_persisted_state_repair_terminal(
        terminal: PersistedStateRepairTerminal,
    ) -> Self {
        let attempt = terminal.attempt();
        let action_outcome = match terminal.outcome() {
            PersistedStateRepairTerminalOutcome::Quarantined => {
                FailureMemoryActionOutcome::Repaired
            }
            PersistedStateRepairTerminalOutcome::Refused
            | PersistedStateRepairTerminalOutcome::AppliedUnverified => {
                FailureMemoryActionOutcome::Failed
            }
        };
        Self {
            key: FailureMemoryKey::for_persisted_state_repair(attempt),
            diagnosis_id: DiagnosisId::PersistedStateSchemaInvalid,
            domain: GuardianDomain::State,
            mode: GuardianMode::Managed,
            target: attempt.target().clone(),
            ownership: OwnershipClass::LauncherManaged,
            first_observed_at: attempt.observed_at().to_string(),
            last_observed_at: attempt.observed_at().to_string(),
            occurrence_count: 1,
            last_action_kind: Some(GuardianActionKind::Quarantine),
            last_action_outcome: Some(action_outcome),
            repair_attempt_count: 1,
            quarantine_checkpoint: ReconciliationQuarantineCheckpoint::default(),
            suppression_until: Some(attempt.suppression_until().to_string()),
            target_content_hash: None,
            user_intent_hash: None,
            reconciliation_terminal: None,
            persisted_state_repair_terminal: Some(terminal),
        }
    }

    pub fn validate(&self) -> Result<(), FailureMemoryValidationError> {
        if !is_safe_memory_fragment(self.key.as_str()) {
            return Err(FailureMemoryValidationError::UnsafeKey);
        }
        if self.reconciliation_terminal.is_some() && self.persisted_state_repair_terminal.is_some()
        {
            return Err(FailureMemoryValidationError::PersistedStateRepairTerminalMismatch);
        }
        let expected_key = if let Some(terminal) = &self.persisted_state_repair_terminal {
            FailureMemoryKey::for_persisted_state_repair(terminal.attempt())
        } else {
            self.reconciliation_terminal.as_ref().map_or_else(
                || {
                    FailureMemoryKey::for_observation(
                        self.domain,
                        &self.diagnosis_id,
                        &self.target,
                        self.mode,
                        self.user_intent_hash.as_deref(),
                    )
                },
                |terminal| {
                    FailureMemoryKey::for_reconciliation(
                        self.domain,
                        &self.diagnosis_id,
                        &self.target,
                        terminal,
                    )
                },
            )
        };
        if self.key != expected_key {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch);
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
        self.quarantine_checkpoint
            .validate_bounded()
            .map_err(|_| FailureMemoryValidationError::UnsafeTargetId)?;
        if let Some(suppression_until) = &self.suppression_until
            && parse_timestamp(suppression_until).is_err()
        {
            return Err(FailureMemoryValidationError::InvalidSuppressionTimestamp);
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
        if let Some(terminal) = &self.reconciliation_terminal {
            terminal
                .validate()
                .map_err(|_| FailureMemoryValidationError::InvalidReconciliationTerminal)?;
            if self.mode != terminal.mode()
                || self.diagnosis_id != terminal.diagnosis_id()
                || self.domain != terminal.domain()
                || self.ownership != terminal.ownership()
                || self.target.ownership != terminal.ownership()
                || &self.target != terminal.target()
                || self.user_intent_hash.is_some()
                || self.last_action_kind != Some(GuardianActionKind::Repair)
                || self.repair_attempt_count == 0
                || self.last_observed_at != terminal.observed_at()
                || self.suppression_until.as_deref() != Some(terminal.suppression_until())
                || &self.quarantine_checkpoint != terminal.quarantine_checkpoint()
            {
                return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch);
            }
            let expected_outcome = match terminal.outcome() {
                ReconciliationTerminalOutcome::Succeeded => FailureMemoryActionOutcome::Repaired,
                ReconciliationTerminalOutcome::Failed => FailureMemoryActionOutcome::Failed,
            };
            if self.last_action_outcome != Some(expected_outcome)
                || self.suppression_until.is_none()
            {
                return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch);
            }
        }
        if let Some(terminal) = &self.persisted_state_repair_terminal {
            terminal
                .validate()
                .map_err(|_| FailureMemoryValidationError::InvalidPersistedStateRepairTerminal)?;
            let attempt = terminal.attempt();
            let expected_outcome = match terminal.outcome() {
                PersistedStateRepairTerminalOutcome::Quarantined => {
                    FailureMemoryActionOutcome::Repaired
                }
                PersistedStateRepairTerminalOutcome::Refused
                | PersistedStateRepairTerminalOutcome::AppliedUnverified => {
                    FailureMemoryActionOutcome::Failed
                }
            };
            if self.diagnosis_id != DiagnosisId::PersistedStateSchemaInvalid
                || self.domain != GuardianDomain::State
                || self.mode != GuardianMode::Managed
                || self.ownership != OwnershipClass::LauncherManaged
                || self.target != *attempt.target()
                || self.user_intent_hash.is_some()
                || self.target_content_hash.is_some()
                || self.last_action_kind != Some(GuardianActionKind::Quarantine)
                || self.last_action_outcome != Some(expected_outcome)
                || self.first_observed_at != attempt.observed_at()
                || self.last_observed_at != attempt.observed_at()
                || self.occurrence_count != 1
                || self.repair_attempt_count != 1
                || self.suppression_until.as_deref() != Some(attempt.suppression_until())
                || !self.quarantine_checkpoint.is_empty()
            {
                return Err(FailureMemoryValidationError::PersistedStateRepairTerminalMismatch);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailureMemoryActionOutcome {
    Repaired,
    Retried,
    Blocked,
    Failed,
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
        if super::persisted_snapshot_schema(value)? != FAILURE_MEMORY_SCHEMA {
            return Err(FailureMemoryLoadError::InvalidSchema);
        }
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
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            if serde_json::to_vec(entry)?.len() as u64 > MAX_FAILURE_MEMORY_ENTRY_BYTES {
                return Err(FailureMemoryLoadError::TooLarge);
            }
            entry.validate()?;
            if !keys.insert(entry.key.as_str()) {
                return Err(FailureMemoryLoadError::DuplicateKey);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum FailureMemoryLoadError {
    Json(serde_json::Error),
    InvalidSchema,
    TooLarge,
    TooManyEntries,
    InvalidEntry(FailureMemoryValidationError),
    DuplicateKey,
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
    UnsafeTargetId,
    UnsafeTargetHash,
    UnsafeUserIntentHash,
    MemoryKeyMismatch,
    OwnershipMismatch,
    ZeroOccurrences,
    InvalidObservedTimestamp,
    InvalidSuppressionTimestamp,
    ObservedTimestampTooFarInFuture,
    SuppressionWindowOutOfBounds,
    InvalidReconciliationTerminal,
    ReconciliationTerminalMismatch,
    InvalidPersistedStateRepairTerminal,
    PersistedStateRepairTerminalMismatch,
    InstallGuardianRetryMismatch,
}

#[derive(Debug, thiserror::Error)]
pub enum FailureMemoryStoreError {
    #[error("invalid Guardian failure-memory entry: {0:?}")]
    Validation(FailureMemoryValidationError),
    #[error("invalid Guardian failure-memory snapshot: {0:?}")]
    Snapshot(FailureMemoryLoadError),
    #[error("Guardian failure-memory persistence failed: {0}")]
    Persistence(#[source] io::Error),
    #[error("Guardian failure-memory capacity is exhausted by active reconciliation evidence")]
    CapacityExhausted,
    #[error("Guardian failure-memory entry is outside the retained temporal window")]
    Expired,
}

impl FailureMemoryStoreError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Snapshot(_) => "snapshot",
            Self::Persistence(_) => "persistence",
            Self::CapacityExhausted => "capacity_exhausted",
            Self::Expired => "expired",
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
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, FailureMemoryStoreError> {
        let record = directory
            .target(
                std::ffi::OsStr::new("failure-memory.json"),
                MAX_FAILURE_MEMORY_SNAPSHOT_BYTES,
            )
            .and_then(|record| FAILURE_MEMORY_SNAPSHOT_SUCCESSOR.bind(record))
            .map_err(FailureMemoryStoreError::Persistence)?;
        let owner = coordinator
            .claim_record(record.clone())
            .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        let writer = owner
            .writer(record)
            .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
        Ok(Self { owner, writer })
    }
}

pub struct GuardianFailureMemoryStore {
    records: Arc<RwLock<FailureMemoryRecords>>,
    attempts: Arc<Mutex<BTreeSet<String>>>,
    install_guardian_settlement: AsyncMutex<()>,
    max_entries: usize,
    temporal: Arc<BoundedTemporalPolicy>,
    temporal_future_observation_count: AtomicUsize,
    temporal_out_of_bounds_window_count: AtomicUsize,
    persistence: Option<FailureMemoryPersistence>,
}

pub(super) struct ReconciliationAttemptReservation {
    key: FailureMemoryKey,
    attempts: Arc<Mutex<BTreeSet<String>>>,
}

pub(super) struct PersistedStateRepairReservation {
    key: FailureMemoryKey,
    attempts: Arc<Mutex<BTreeSet<String>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReconciliationAttemptReserveError {
    PersistencePending,
    AlreadyReserved,
    CapacityExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PersistedStateRepairReserveError {
    InvalidAttempt,
    PersistencePending,
    AlreadyReserved,
    Suppressed,
    CapacityExhausted,
}

impl Drop for ReconciliationAttemptReservation {
    fn drop(&mut self) {
        self.attempts
            .lock()
            .expect("Guardian reconciliation attempts lock poisoned")
            .remove(self.key.as_str());
    }
}

impl Drop for PersistedStateRepairReservation {
    fn drop(&mut self) {
        self.attempts
            .lock()
            .expect("Guardian persisted-state repair attempts lock poisoned")
            .remove(self.key.as_str());
    }
}

#[derive(Default)]
struct FailureMemoryRecords {
    visible: BTreeMap<String, GuardianFailureMemoryEntry>,
    visible_revision: u64,
    retry_candidate: Option<(u64, BTreeMap<String, GuardianFailureMemoryEntry>)>,
    critical_pending: bool,
}

struct PendingFailureMemoryCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: BTreeMap<String, GuardianFailureMemoryEntry>,
}

impl GuardianFailureMemoryStore {
    pub fn new() -> Self {
        Self::with_max_entries(DEFAULT_FAILURE_MEMORY_LIMIT)
    }

    pub fn with_max_entries(max_entries: usize) -> Self {
        Self::with_max_entries_and_clock(
            max_entries,
            Arc::new(BoundedTemporalPolicy::system()),
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn at_for_test(wall: DateTime<Utc>) -> Self {
        Self::with_max_entries_and_clock(
            DEFAULT_FAILURE_MEMORY_LIMIT,
            Arc::new(BoundedTemporalPolicy::fixed(wall)),
            None,
        )
    }

    fn with_max_entries_and_clock(
        max_entries: usize,
        temporal: Arc<BoundedTemporalPolicy>,
        persistence: Option<FailureMemoryPersistence>,
    ) -> Self {
        Self {
            records: Arc::new(RwLock::new(FailureMemoryRecords::default())),
            attempts: Arc::new(Mutex::new(BTreeSet::new())),
            install_guardian_settlement: AsyncMutex::new(()),
            max_entries: max_entries.clamp(1, DEFAULT_FAILURE_MEMORY_LIMIT),
            temporal,
            temporal_future_observation_count: AtomicUsize::new(0),
            temporal_out_of_bounds_window_count: AtomicUsize::new(0),
            persistence,
        }
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_directory(
        directory: AnchoredRecordDirectory,
    ) -> Result<Self, FailureMemoryStoreError> {
        Self::try_load_from_directory_with_temporal(
            directory,
            Arc::new(BoundedTemporalPolicy::system()),
        )
    }

    pub(crate) fn try_load_from_directory_with_temporal(
        directory: AnchoredRecordDirectory,
        temporal: Arc<BoundedTemporalPolicy>,
    ) -> Result<Self, FailureMemoryStoreError> {
        Self::try_load_with_coordinator_directory_and_temporal(
            PersistenceCoordinator::current(),
            directory,
            temporal,
        )
    }

    #[cfg(test)]
    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, FailureMemoryStoreError> {
        let directory = test_failure_memory_record_directory(paths)?;
        Self::try_load_from_directory(directory)
    }

    #[cfg(test)]
    pub(crate) fn try_load_from_directory_with_coordinator(
        directory: AnchoredRecordDirectory,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, FailureMemoryStoreError> {
        Self::try_load_with_coordinator_directory_and_temporal(
            coordinator,
            directory,
            Arc::new(BoundedTemporalPolicy::system()),
        )
    }

    fn try_load_with_coordinator_directory_and_temporal(
        coordinator: PersistenceCoordinator,
        directory: AnchoredRecordDirectory,
        temporal: Arc<BoundedTemporalPolicy>,
    ) -> Result<Self, FailureMemoryStoreError> {
        let store = Self::with_max_entries_and_clock(
            DEFAULT_FAILURE_MEMORY_LIMIT,
            temporal,
            Some(FailureMemoryPersistence::claim(
                directory.clone(),
                coordinator,
            )?),
        );

        store.load_from_directory(&directory)?;

        Ok(store)
    }

    fn load_from_directory(
        &self,
        directory: &AnchoredRecordDirectory,
    ) -> Result<(), FailureMemoryStoreError> {
        let observation = match directory.read(
            std::ffi::OsStr::new("failure-memory.json"),
            MAX_FAILURE_MEMORY_SNAPSHOT_BYTES,
        ) {
            Ok(observation) => observation,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(FailureMemoryStoreError::Persistence(error)),
        };
        let bytes = observation
            .bytes()
            .ok_or(FailureMemoryLoadError::TooLarge)?;
        let data = String::from_utf8(bytes.to_vec()).map_err(|error| {
            FailureMemoryStoreError::Persistence(io::Error::new(io::ErrorKind::InvalidData, error))
        })?;
        self.load_snapshot(FailureMemorySnapshot::from_json(&data)?)?;
        observation
            .admit(MAX_FAILURE_MEMORY_SNAPSHOT_BYTES)
            .map_err(FailureMemoryStoreError::Persistence)?;
        Ok(())
    }

    pub(crate) async fn lock_install_guardian_settlement(&self) -> AsyncMutexGuard<'_, ()> {
        self.install_guardian_settlement.lock().await
    }

    pub(crate) fn construct_entry(
        &self,
        entry: GuardianFailureMemoryEntry,
    ) -> Result<GuardianFailureMemoryEntry, FailureMemoryStoreError> {
        match assess_temporal(&self.temporal, &entry)? {
            BoundedTemporalDisposition::Current => Ok(entry),
            BoundedTemporalDisposition::Expired => Err(FailureMemoryStoreError::Expired),
        }
    }

    pub(crate) async fn settle_install_guardian_pending(
        &self,
    ) -> Result<(), FailureMemoryStoreError> {
        let (critical_pending, retry_pending) = {
            let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
            (records.critical_pending, records.retry_candidate.is_some())
        };
        if retry_pending {
            return self.retry().await;
        }
        if critical_pending {
            return self.flush().await;
        }
        Ok(())
    }

    pub(crate) async fn record_install_guardian_retry(
        &self,
        entry: GuardianFailureMemoryEntry,
    ) -> Result<(), FailureMemoryStoreError> {
        let pending = self.record_with(entry, apply_record, WriteUrgency::Immediate)?;
        self.await_commit(pending).await
    }

    pub(crate) async fn reconcile_install_guardian_retry(
        &self,
        entry: GuardianFailureMemoryEntry,
    ) -> Result<(), FailureMemoryStoreError> {
        self.reconcile_install_guardian_retry_batch(vec![entry])
            .await
    }

    pub(crate) async fn reconcile_install_guardian_retry_batch(
        &self,
        entries: Vec<GuardianFailureMemoryEntry>,
    ) -> Result<(), FailureMemoryStoreError> {
        let pending = self.reconcile_install_guardian_retry_batch_candidate(entries)?;
        self.await_commit(pending).await
    }

    fn reconcile_install_guardian_retry_batch_candidate(
        &self,
        entries: Vec<GuardianFailureMemoryEntry>,
    ) -> Result<Option<PendingFailureMemoryCommit>, FailureMemoryStoreError> {
        let mut protected_keys = BTreeSet::new();
        for entry in &entries {
            match assess_temporal(&self.temporal, entry)? {
                BoundedTemporalDisposition::Current => {}
                BoundedTemporalDisposition::Expired => {
                    return Err(FailureMemoryStoreError::Expired);
                }
            }
            install_guardian_retry_observation(entry)?;
            if !protected_keys.insert(entry.key.as_str().to_string()) {
                return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
            }
        }
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        if records.retry_candidate.is_some() || records.critical_pending {
            return Err(FailureMemoryStoreError::Persistence(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Guardian failure-memory persistence requires retry",
            )));
        }
        let mut candidate = records.visible.clone();
        prune_expired_records(&mut candidate, &self.temporal);
        let mut changed = false;
        for entry in entries {
            changed |= apply_install_guardian_startup_retry(&mut candidate, entry)?;
        }
        if !changed {
            return Ok(None);
        }
        if !prune_records_protecting(
            &mut candidate,
            self.max_entries,
            &protected_keys,
            &self.temporal,
        ) {
            return Err(FailureMemoryStoreError::CapacityExhausted);
        }
        let snapshot = FailureMemorySnapshot::new(candidate.values().cloned().collect())?;
        if self.persistence.is_some() {
            records.critical_pending = true;
        }
        let ticket = match self.persistence.as_ref().map(|persistence| {
            persistence
                .writer
                .accept(snapshot, WriteUrgency::Immediate, encode_snapshot)
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))
        }) {
            Some(Ok(ticket)) => Some(ticket),
            Some(Err(error)) => {
                records.critical_pending = false;
                return Err(error);
            }
            None => None,
        };
        let Some(ticket) = ticket else {
            records.visible = candidate;
            return Ok(None);
        };
        Ok(Some(PendingFailureMemoryCommit {
            revision: ticket.revision().get(),
            ticket,
            candidate,
        }))
    }

    /// Accepts the updated snapshot for persistence before publishing it in memory.
    ///
    /// Success means the revision is owned by the persistence coordinator. Call
    /// [`Self::flush`] when the physical write must be observed before continuing.
    pub fn record(&self, entry: GuardianFailureMemoryEntry) -> Result<(), FailureMemoryStoreError> {
        self.record_with(entry, apply_record, WriteUrgency::Debounced)
            .map(|pending| debug_assert!(pending.is_none()))
    }

    fn record_with(
        &self,
        entry: GuardianFailureMemoryEntry,
        apply: impl FnOnce(
            &mut BTreeMap<String, GuardianFailureMemoryEntry>,
            GuardianFailureMemoryEntry,
        ),
        urgency: WriteUrgency,
    ) -> Result<Option<PendingFailureMemoryCommit>, FailureMemoryStoreError> {
        self.record_with_checked(entry, urgency, false, false, |records, entry| {
            apply(records, entry);
            Ok(true)
        })
    }

    fn record_with_checked(
        &self,
        entry: GuardianFailureMemoryEntry,
        urgency: WriteUrgency,
        protect_entry: bool,
        allow_expired_transition: bool,
        apply: impl FnOnce(
            &mut BTreeMap<String, GuardianFailureMemoryEntry>,
            GuardianFailureMemoryEntry,
        ) -> Result<bool, FailureMemoryStoreError>,
    ) -> Result<Option<PendingFailureMemoryCommit>, FailureMemoryStoreError> {
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        if records.retry_candidate.is_some() || records.critical_pending {
            return Err(FailureMemoryStoreError::Persistence(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Guardian failure-memory persistence requires retry",
            )));
        }
        match assess_temporal(&self.temporal, &entry)? {
            BoundedTemporalDisposition::Current => {}
            BoundedTemporalDisposition::Expired if !allow_expired_transition => {
                return Err(FailureMemoryStoreError::Expired);
            }
            BoundedTemporalDisposition::Expired => {}
        }
        let protected_key = (protect_entry
            || entry.reconciliation_terminal().is_some()
            || entry.persisted_state_repair_terminal().is_some())
        .then(|| entry.key.as_str().to_string());
        let mut candidate = records.visible.clone();
        prune_expired_records(&mut candidate, &self.temporal);
        if !apply(&mut candidate, entry)? {
            return Ok(None);
        }
        if !prune_records(
            &mut candidate,
            self.max_entries,
            protected_key.as_deref(),
            &self.temporal,
        ) {
            return Err(FailureMemoryStoreError::CapacityExhausted);
        }
        let snapshot = FailureMemorySnapshot::new(candidate.values().cloned().collect())?;
        if urgency == WriteUrgency::Immediate && self.persistence.is_some() {
            records.critical_pending = true;
        }
        let ticket = match self.persistence.as_ref().map(|persistence| {
            persistence
                .writer
                .accept(snapshot, urgency, encode_snapshot)
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))
        }) {
            Some(Ok(ticket)) => Some(ticket),
            Some(Err(error)) => {
                records.critical_pending = false;
                return Err(error);
            }
            None => None,
        };
        let Some(ticket) = ticket else {
            records.visible = candidate;
            return Ok(None);
        };
        let revision = ticket.revision().get();
        if urgency == WriteUrgency::Debounced {
            records.visible = candidate;
            records.visible_revision = revision;
            return Ok(None);
        }
        Ok(Some(PendingFailureMemoryCommit {
            ticket,
            revision,
            candidate,
        }))
    }

    pub(super) async fn record_reconciliation_terminal(
        &self,
        entry: GuardianFailureMemoryEntry,
        reservation: &ReconciliationAttemptReservation,
    ) -> Result<(), FailureMemoryStoreError> {
        self.validate_reconciliation_terminal(&entry, reservation)?;
        let key = entry.key.clone();
        if let Some(stored) = self.get(&key)
            && stored == entry
        {
            return Ok(());
        }
        if self
            .records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .retry_candidate
            .is_some()
        {
            self.retry().await?;
            self.validate_reconciliation_terminal(&entry, reservation)?;
            if let Some(stored) = self.get(&key)
                && stored == entry
            {
                return Ok(());
            }
        }
        let pending = self.record_with_checked(
            entry,
            WriteUrgency::Immediate,
            false,
            true,
            |records, entry| {
                apply_reconciliation_record(records, entry);
                Ok(true)
            },
        )?;
        self.await_commit(pending).await
    }

    pub(super) async fn acknowledge_reconciliation_version_bundle_publication(
        &self,
        expected: &GuardianFailureMemoryEntry,
        acknowledged: GuardianFailureMemoryEntry,
    ) -> Result<(), FailureMemoryStoreError> {
        let Some(expected_terminal) = expected.reconciliation_terminal() else {
            return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into());
        };
        let Some(evidence) = expected_terminal
            .version_bundle_publication()
            .filter(|publication| publication.is_pending())
            .map(|publication| publication.evidence())
        else {
            return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into());
        };
        let exact_terminal = expected_terminal
            .clone()
            .with_acknowledged_version_bundle_publication(evidence)
            .map_err(|_| FailureMemoryValidationError::ReconciliationTerminalMismatch)?;
        let mut exact_acknowledgement = expected.clone();
        exact_acknowledgement.reconciliation_terminal = Some(exact_terminal);
        if acknowledged != exact_acknowledgement {
            return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into());
        }
        let pending = self.record_with_checked(
            acknowledged.clone(),
            WriteUrgency::Immediate,
            false,
            true,
            |records, replacement| match records.get(expected.key.as_str()) {
                Some(current) if current == &replacement => Ok(false),
                Some(current) if current == expected => {
                    records.insert(replacement.key.as_str().to_string(), replacement);
                    Ok(true)
                }
                _ => Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into()),
            },
        )?;
        self.await_commit(pending).await
    }

    pub(super) async fn record_persisted_state_repair_terminal(
        &self,
        entry: GuardianFailureMemoryEntry,
        reservation: &PersistedStateRepairReservation,
    ) -> Result<(), FailureMemoryStoreError> {
        self.validate_persisted_state_repair_terminal(&entry, reservation)?;
        let key = entry.key.clone();
        if self.get(&key).is_some_and(|stored| stored == entry) {
            return Ok(());
        }
        if self
            .records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .retry_candidate
            .is_some()
        {
            self.retry().await?;
            self.validate_persisted_state_repair_terminal(&entry, reservation)?;
            if self.get(&key).is_some_and(|stored| stored == entry) {
                return Ok(());
            }
        }
        let pending = self.record_with_checked(
            entry,
            WriteUrgency::Immediate,
            false,
            true,
            |records, entry| {
                apply_reconciliation_record(records, entry);
                Ok(true)
            },
        )?;
        self.await_commit(pending).await
    }

    pub(super) fn validate_persisted_state_repair_terminal(
        &self,
        entry: &GuardianFailureMemoryEntry,
        reservation: &PersistedStateRepairReservation,
    ) -> Result<(), FailureMemoryStoreError> {
        if entry.persisted_state_repair_terminal().is_none() {
            return Err(FailureMemoryValidationError::InvalidPersistedStateRepairTerminal.into());
        }
        match assess_temporal(&self.temporal, entry)? {
            BoundedTemporalDisposition::Current | BoundedTemporalDisposition::Expired => {}
        }
        if reservation.key != entry.key || !Arc::ptr_eq(&reservation.attempts, &self.attempts) {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch.into());
        }
        let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        let attempts = self
            .attempts
            .lock()
            .expect("Guardian persisted-state repair attempts lock poisoned");
        if !attempts.contains(entry.key.as_str()) {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch.into());
        }
        if records
            .visible
            .get(entry.key.as_str())
            .is_some_and(|stored| {
                stored != entry
                    && !persisted_state_repair_entry_can_be_superseded(
                        stored,
                        entry,
                        &self.temporal,
                    )
            })
        {
            return Err(FailureMemoryValidationError::PersistedStateRepairTerminalMismatch.into());
        }
        let mut candidate = records.visible.clone();
        apply_reconciliation_record(&mut candidate, entry.clone());
        prune_expired_records(&mut candidate, &self.temporal);
        if !prune_records(
            &mut candidate,
            self.max_entries,
            Some(entry.key.as_str()),
            &self.temporal,
        ) {
            return Err(FailureMemoryStoreError::CapacityExhausted);
        }
        Ok(())
    }

    pub(super) fn validate_reconciliation_terminal(
        &self,
        entry: &GuardianFailureMemoryEntry,
        reservation: &ReconciliationAttemptReservation,
    ) -> Result<(), FailureMemoryStoreError> {
        if entry.reconciliation_terminal().is_none() {
            return Err(FailureMemoryValidationError::InvalidReconciliationTerminal.into());
        }
        match assess_temporal(&self.temporal, entry)? {
            BoundedTemporalDisposition::Current | BoundedTemporalDisposition::Expired => {}
        }
        if reservation.key != entry.key || !Arc::ptr_eq(&reservation.attempts, &self.attempts) {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch.into());
        }

        let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        let attempts = self
            .attempts
            .lock()
            .expect("Guardian reconciliation attempts lock poisoned");
        if !attempts.contains(entry.key.as_str()) {
            return Err(FailureMemoryValidationError::MemoryKeyMismatch.into());
        }
        if let Some(stored) = records.visible.get(entry.key.as_str())
            && stored != entry
            && !reconciliation_entry_can_be_superseded(stored, entry, &self.temporal)
        {
            return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into());
        }

        let mut candidate = records.visible.clone();
        apply_reconciliation_record(&mut candidate, entry.clone());
        prune_expired_records(&mut candidate, &self.temporal);
        if !prune_records(
            &mut candidate,
            self.max_entries,
            Some(entry.key.as_str()),
            &self.temporal,
        ) {
            return Err(FailureMemoryStoreError::CapacityExhausted);
        }
        Ok(())
    }

    pub(super) fn reserve_reconciliation_attempt(
        &self,
        key: FailureMemoryKey,
    ) -> Result<ReconciliationAttemptReservation, ReconciliationAttemptReserveError> {
        let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        if records.critical_pending || records.retry_candidate.is_some() {
            return Err(ReconciliationAttemptReserveError::PersistencePending);
        }
        let mut attempts = self
            .attempts
            .lock()
            .expect("Guardian reconciliation attempts lock poisoned");
        if attempts.contains(key.as_str()) {
            return Err(ReconciliationAttemptReserveError::AlreadyReserved);
        }
        let mut occupied_keys = attempts.clone();
        occupied_keys.extend(
            records
                .visible
                .values()
                .filter(|entry| active_durable_terminal(entry, &self.temporal))
                .map(|entry| entry.key.as_str().to_string()),
        );
        if !occupied_keys.contains(key.as_str()) && occupied_keys.len() >= self.max_entries {
            return Err(ReconciliationAttemptReserveError::CapacityExhausted);
        }
        attempts.insert(key.as_str().to_string());
        drop(records);
        Ok(ReconciliationAttemptReservation {
            key,
            attempts: self.attempts.clone(),
        })
    }

    pub(super) fn reserve_persisted_state_repair(
        &self,
        attempt: &PersistedStateRepairAttempt,
    ) -> Result<PersistedStateRepairReservation, PersistedStateRepairReserveError> {
        attempt
            .validate()
            .map_err(|_| PersistedStateRepairReserveError::InvalidAttempt)?;
        match self.temporal.assess(BoundedTemporalRecord {
            first_observed_at: attempt.observed_at(),
            last_observed_at: attempt.observed_at(),
            suppression_until: Some(attempt.suppression_until()),
            pending: false,
        }) {
            Ok(BoundedTemporalDisposition::Current) => {}
            Ok(BoundedTemporalDisposition::Expired) | Err(_) => {
                return Err(PersistedStateRepairReserveError::InvalidAttempt);
            }
        }
        let key = FailureMemoryKey::for_persisted_state_repair(attempt);
        let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        if records.critical_pending || records.retry_candidate.is_some() {
            return Err(PersistedStateRepairReserveError::PersistencePending);
        }
        let mut attempts = self
            .attempts
            .lock()
            .expect("Guardian persisted-state repair attempts lock poisoned");
        if attempts.contains(key.as_str()) {
            return Err(PersistedStateRepairReserveError::AlreadyReserved);
        }
        if records
            .visible
            .get(key.as_str())
            .is_some_and(|entry| self.temporal.suppression_active(temporal_record(entry)))
        {
            return Err(PersistedStateRepairReserveError::Suppressed);
        }
        let mut occupied_keys = attempts.clone();
        occupied_keys.extend(
            records
                .visible
                .values()
                .filter(|entry| active_durable_terminal(entry, &self.temporal))
                .map(|entry| entry.key.as_str().to_string()),
        );
        if !occupied_keys.contains(key.as_str()) && occupied_keys.len() >= self.max_entries {
            return Err(PersistedStateRepairReserveError::CapacityExhausted);
        }
        attempts.insert(key.as_str().to_string());
        Ok(PersistedStateRepairReservation {
            key,
            attempts: self.attempts.clone(),
        })
    }

    pub(crate) async fn settle_reconciliation_pending(
        &self,
    ) -> Result<(), FailureMemoryStoreError> {
        let retry_pending = {
            let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
            records.retry_candidate.is_some()
        };
        if retry_pending {
            return self.retry().await;
        }
        match self.flush().await {
            Ok(()) => Ok(()),
            Err(_) => self.retry().await,
        }
    }

    pub fn get(&self, key: &FailureMemoryKey) -> Option<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .visible
            .get(key.as_str())
            .cloned()
    }

    pub fn list(&self) -> Vec<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .visible
            .values()
            .cloned()
            .collect()
    }

    pub fn list_current(&self) -> Vec<GuardianFailureMemoryEntry> {
        self.records
            .read()
            .expect(FAILURE_MEMORY_LOCK_INVARIANT)
            .visible
            .values()
            .filter(|entry| {
                assess_temporal(&self.temporal, entry)
                    .is_ok_and(|disposition| disposition == BoundedTemporalDisposition::Current)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn suppression_active(&self, entry: &GuardianFailureMemoryEntry) -> bool {
        entry.validate().is_ok() && self.temporal.suppression_active(temporal_record(entry))
    }

    pub(crate) fn now_timestamp(&self) -> String {
        self.temporal.now_timestamp()
    }

    pub fn temporal_quarantine_count(&self) -> usize {
        self.temporal_load_issues().total()
    }

    pub(crate) fn temporal_load_issues(&self) -> BoundedTemporalLoadIssueCounts {
        BoundedTemporalLoadIssueCounts::new(
            self.temporal_future_observation_count
                .load(Ordering::Acquire),
            self.temporal_out_of_bounds_window_count
                .load(Ordering::Acquire),
        )
    }

    pub fn snapshot(&self) -> Result<FailureMemorySnapshot, FailureMemoryLoadError> {
        FailureMemorySnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: FailureMemorySnapshot,
    ) -> Result<(), FailureMemoryLoadError> {
        snapshot.validate()?;
        let mut candidate = BTreeMap::new();
        let mut expired_publication_carriers = BTreeSet::new();
        let mut temporal_load_issues = BoundedTemporalLoadIssueCounts::default();
        for entry in snapshot.entries {
            match assess_temporal(&self.temporal, &entry) {
                Ok(BoundedTemporalDisposition::Current) => {}
                Ok(BoundedTemporalDisposition::Expired)
                    if entry
                        .reconciliation_terminal()
                        .and_then(ReconciliationTerminal::version_bundle_publication)
                        .is_some() =>
                {
                    expired_publication_carriers.insert(entry.key.as_str().to_string());
                }
                Ok(BoundedTemporalDisposition::Expired) => continue,
                Err(FailureMemoryValidationError::ObservedTimestampTooFarInFuture) => {
                    temporal_load_issues
                        .record(BoundedTemporalViolation::ObservationTooFarInFuture);
                    continue;
                }
                Err(FailureMemoryValidationError::SuppressionWindowOutOfBounds) => {
                    temporal_load_issues
                        .record(BoundedTemporalViolation::SuppressionWindowOutOfBounds);
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            candidate.insert(entry.key.as_str().to_string(), entry);
        }
        if !prune_records_protecting(
            &mut candidate,
            self.max_entries,
            &expired_publication_carriers,
            &self.temporal,
        ) {
            return Err(FailureMemoryLoadError::TooManyEntries);
        }
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        records.visible = candidate;
        records.visible_revision = 0;
        records.retry_candidate = None;
        records.critical_pending = false;
        self.temporal_future_observation_count
            .store(temporal_load_issues.future_observation(), Ordering::Release);
        self.temporal_out_of_bounds_window_count.store(
            temporal_load_issues.out_of_bounds_window(),
            Ordering::Release,
        );
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
            let ticket = persistence
                .writer
                .retry()
                .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
            let revision = ticket.revision().get();
            let candidate = self
                .records
                .read()
                .expect(FAILURE_MEMORY_LOCK_INVARIANT)
                .retry_candidate
                .as_ref()
                .filter(|(candidate_revision, _)| *candidate_revision == revision)
                .map(|(_, candidate)| candidate.clone());
            if let Some(candidate) = candidate {
                self.await_commit(Some(PendingFailureMemoryCommit {
                    ticket,
                    revision,
                    candidate,
                }))
                .await?;
            } else {
                ticket
                    .persisted()
                    .await
                    .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))?;
            }
        }
        Ok(())
    }

    async fn await_commit(
        &self,
        commit: Option<PendingFailureMemoryCommit>,
    ) -> Result<(), FailureMemoryStoreError> {
        let Some(commit) = commit else {
            return Ok(());
        };
        let records = self.records.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut records = records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
                    if records.visible_revision < commit.revision {
                        records.visible = commit.candidate;
                        records.visible_revision = commit.revision;
                    }
                    records.retry_candidate = None;
                    records.critical_pending = false;
                    Ok(())
                }
                Err(error) => {
                    records
                        .write()
                        .expect(FAILURE_MEMORY_LOCK_INVARIANT)
                        .retry_candidate = Some((commit.revision, commit.candidate));
                    Err(error)
                }
            };
            let _ = completed_tx.send(result);
        });
        completed_rx
            .await
            .map_err(|_| {
                FailureMemoryStoreError::Persistence(io::Error::other(
                    "Guardian failure-memory commit observer stopped",
                ))
            })?
            .map_err(|error| FailureMemoryStoreError::Persistence(error.into()))
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

#[cfg(test)]
fn test_failure_memory_record_directory(
    paths: &AppPaths,
) -> Result<AnchoredRecordDirectory, FailureMemoryStoreError> {
    let root_session = crate::state::test_root_session(paths);
    let directory = root_session
        .prepare_persisted_state_directories()
        .map(|directories| directories.guardian_failure_memory_parent())
        .map_err(FailureMemoryStoreError::Persistence)?;
    Ok(AnchoredRecordDirectory::from_directory(
        root_session,
        directory,
    ))
}

impl Default for GuardianFailureMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn prune_records(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    max_entries: usize,
    protected_key: Option<&str>,
    temporal: &BoundedTemporalPolicy,
) -> bool {
    let protected_keys = protected_key
        .map(str::to_string)
        .into_iter()
        .collect::<BTreeSet<_>>();
    prune_records_protecting(records, max_entries, &protected_keys, temporal)
}

fn prune_records_protecting(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    max_entries: usize,
    protected_keys: &BTreeSet<String>,
    temporal: &BoundedTemporalPolicy,
) -> bool {
    records.retain(|key, entry| {
        protected_keys.contains(key)
            || assess_temporal(temporal, entry)
                .is_ok_and(|disposition| disposition == BoundedTemporalDisposition::Current)
    });
    if records.len() <= max_entries {
        return true;
    }

    let mut ordered = records
        .values()
        .filter(|entry| {
            !protected_keys.contains(entry.key.as_str())
                && !active_durable_terminal(entry, temporal)
        })
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
    records.len() <= max_entries
}

fn prune_expired_records(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    temporal: &BoundedTemporalPolicy,
) {
    records.retain(|_, entry| {
        assess_temporal(temporal, entry)
            .is_ok_and(|disposition| disposition == BoundedTemporalDisposition::Current)
    });
}

fn active_durable_terminal(
    entry: &GuardianFailureMemoryEntry,
    temporal: &BoundedTemporalPolicy,
) -> bool {
    entry.has_pending_publication()
        || (entry.reconciliation_terminal().is_some()
            || entry.persisted_state_repair_terminal().is_some())
            && temporal.suppression_active(temporal_record(entry))
}

fn temporal_record(entry: &GuardianFailureMemoryEntry) -> BoundedTemporalRecord<'_> {
    BoundedTemporalRecord {
        first_observed_at: &entry.first_observed_at,
        last_observed_at: &entry.last_observed_at,
        suppression_until: entry.suppression_until.as_deref(),
        pending: entry.has_pending_publication(),
    }
}

fn assess_temporal(
    temporal: &BoundedTemporalPolicy,
    entry: &GuardianFailureMemoryEntry,
) -> Result<BoundedTemporalDisposition, FailureMemoryValidationError> {
    entry.validate()?;
    temporal
        .assess(temporal_record(entry))
        .map_err(|violation| match violation {
            BoundedTemporalViolation::MalformedTimestamp => {
                FailureMemoryValidationError::InvalidObservedTimestamp
            }
            BoundedTemporalViolation::ObservationTooFarInFuture => {
                FailureMemoryValidationError::ObservedTimestampTooFarInFuture
            }
            BoundedTemporalViolation::SuppressionWindowOutOfBounds => {
                FailureMemoryValidationError::SuppressionWindowOutOfBounds
            }
        })
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

fn apply_install_guardian_startup_retry(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    replacement: GuardianFailureMemoryEntry,
) -> Result<bool, FailureMemoryStoreError> {
    let replacement_observed = install_guardian_retry_observation(&replacement)?;
    let key = replacement.key.as_str().to_string();
    let Some(existing) = records.get_mut(&key) else {
        records.insert(key, replacement);
        return Ok(true);
    };
    let existing_first_observed =
        canonical_install_guardian_retry_timestamp(&existing.first_observed_at)?;
    let existing_last_observed =
        canonical_install_guardian_retry_timestamp(&existing.last_observed_at)?;
    let existing_suppression_until = existing
        .suppression_until
        .as_deref()
        .ok_or(FailureMemoryValidationError::InstallGuardianRetryMismatch)
        .and_then(canonical_install_guardian_retry_timestamp)?;
    if existing.reconciliation_terminal().is_some()
        || existing.persisted_state_repair_terminal().is_some()
        || existing.last_action_kind != Some(GuardianActionKind::Retry)
        || existing_last_observed.checked_add_signed(chrono::Duration::minutes(5))
            != Some(existing_suppression_until)
        || !install_guardian_retry_identity_matches(existing, &replacement)
    {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
    }

    if existing == &replacement {
        return Ok(false);
    }
    if existing.last_observed_at == replacement.last_observed_at
        && existing.suppression_until == replacement.suppression_until
    {
        if existing_first_observed <= replacement_observed && existing.occurrence_count >= 1 {
            return Ok(false);
        }
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
    }
    if existing_suppression_until > replacement_observed {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
    }

    let occurrence_count = existing
        .occurrence_count
        .checked_add(1)
        .ok_or(FailureMemoryValidationError::InstallGuardianRetryMismatch)?;
    let first_observed_at = existing.first_observed_at.clone();
    *existing = replacement;
    existing.first_observed_at = first_observed_at;
    existing.occurrence_count = occurrence_count;
    existing.validate()?;
    Ok(true)
}

fn install_guardian_retry_observation(
    entry: &GuardianFailureMemoryEntry,
) -> Result<DateTime<Utc>, FailureMemoryStoreError> {
    if entry.occurrence_count != 1
        || entry.first_observed_at != entry.last_observed_at
        || entry.last_action_kind != Some(GuardianActionKind::Retry)
        || entry.reconciliation_terminal().is_some()
        || entry.persisted_state_repair_terminal().is_some()
    {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
    }
    let observed_at = canonical_install_guardian_retry_timestamp(&entry.first_observed_at)?;
    let suppression_until = entry
        .suppression_until
        .as_deref()
        .ok_or(FailureMemoryValidationError::InstallGuardianRetryMismatch)
        .and_then(canonical_install_guardian_retry_timestamp)?;
    if observed_at.checked_add_signed(chrono::Duration::minutes(5)) != Some(suppression_until) {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch.into());
    }
    Ok(observed_at)
}

fn canonical_install_guardian_retry_timestamp(
    value: &str,
) -> Result<DateTime<Utc>, FailureMemoryValidationError> {
    if value.len() > 40 {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch);
    }
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|_| FailureMemoryValidationError::InstallGuardianRetryMismatch)?
        .with_timezone(&Utc);
    if parsed.to_rfc3339() != value && parsed.to_rfc3339_opts(SecondsFormat::Millis, true) != value
    {
        return Err(FailureMemoryValidationError::InstallGuardianRetryMismatch);
    }
    Ok(parsed)
}

fn install_guardian_retry_identity_matches(
    existing: &GuardianFailureMemoryEntry,
    replacement: &GuardianFailureMemoryEntry,
) -> bool {
    existing.key == replacement.key
        && existing.diagnosis_id == replacement.diagnosis_id
        && existing.domain == replacement.domain
        && existing.mode == replacement.mode
        && existing.target == replacement.target
        && existing.ownership == replacement.ownership
        && existing.last_action_kind == replacement.last_action_kind
        && existing.last_action_outcome == replacement.last_action_outcome
        && existing.repair_attempt_count == replacement.repair_attempt_count
        && existing.quarantine_checkpoint == replacement.quarantine_checkpoint
        && existing.target_content_hash == replacement.target_content_hash
        && existing.user_intent_hash == replacement.user_intent_hash
        && existing.reconciliation_terminal == replacement.reconciliation_terminal
        && existing.persisted_state_repair_terminal == replacement.persisted_state_repair_terminal
}

fn apply_reconciliation_record(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    entry: GuardianFailureMemoryEntry,
) {
    records.insert(entry.key.as_str().to_string(), entry);
}

fn reconciliation_entry_can_be_superseded(
    existing: &GuardianFailureMemoryEntry,
    replacement: &GuardianFailureMemoryEntry,
    temporal: &BoundedTemporalPolicy,
) -> bool {
    let (Some(existing_terminal), Some(replacement_terminal)) = (
        existing.reconciliation_terminal(),
        replacement.reconciliation_terminal(),
    ) else {
        return false;
    };
    if existing_terminal == replacement_terminal {
        return false;
    }
    !temporal.suppression_active(temporal_record(existing))
}

fn persisted_state_repair_entry_can_be_superseded(
    existing: &GuardianFailureMemoryEntry,
    replacement: &GuardianFailureMemoryEntry,
    temporal: &BoundedTemporalPolicy,
) -> bool {
    let (Some(existing_terminal), Some(replacement_terminal)) = (
        existing.persisted_state_repair_terminal(),
        replacement.persisted_state_repair_terminal(),
    ) else {
        return false;
    };
    if existing.key != replacement.key || existing_terminal == replacement_terminal {
        return false;
    }
    !temporal.suppression_active(temporal_record(existing))
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
pub(crate) fn failure_memory_path(paths: &AppPaths) -> PathBuf {
    paths.guardian_failure_memory_file().to_path_buf()
}

fn encode_snapshot(snapshot: FailureMemorySnapshot) -> io::Result<Vec<u8>> {
    let encoded = snapshot
        .to_json()
        .map(String::into_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if encoded.len() as u64 > MAX_FAILURE_MEMORY_SNAPSHOT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Guardian failure-memory snapshot exceeds its persistence bound",
        ));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::{
        FailureMemoryActionOutcome, FailureMemoryLoadError, FailureMemorySnapshot,
        FailureMemoryStoreError, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
        ReconciliationAttemptReserveError, test_failure_memory_record_directory,
    };
    use crate::execution::anchored_record::AnchoredRecordDirectory;
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
    use crate::state::contracts::{
        OperationId, OwnershipClass, PersistedStateRecordStore, PersistedStateRepairAttempt,
        PersistedStateRepairTerminal, PersistedStateRepairTerminalOutcome, ReconciliationAttempt,
        ReconciliationComponent, ReconciliationIncarnationFingerprint,
        ReconciliationInventoryFingerprint, ReconciliationLineage,
        ReconciliationQuarantineCheckpoint, ReconciliationRung, ReconciliationScope,
        ReconciliationTerminal, ReconciliationTerminalOutcome, ReconciliationVersionBundleOutcome,
        RestartStableRecordIdentity, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use crate::state::journals::DEFAULT_OPERATION_JOURNAL_LIMIT;
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
    use crate::state::reconciliation_memory_entry;
    use axial_config::AppPaths;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    const FAILURE_MEMORY_V7_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guardian/failure-memory-v7.json"
    ));

    struct CountingFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
    }

    struct TestFailureMemoryClock {
        reading: Mutex<crate::state::temporal::TemporalClockReading>,
    }

    impl TestFailureMemoryClock {
        fn new(wall: chrono::DateTime<chrono::Utc>) -> Self {
            Self {
                reading: Mutex::new(crate::state::temporal::TemporalClockReading {
                    wall,
                    monotonic: Duration::ZERO,
                }),
            }
        }

        fn set_wall(&self, wall: chrono::DateTime<chrono::Utc>) {
            self.reading.lock().expect("test clock lock").wall = wall;
        }

        fn set_monotonic(&self, monotonic: Duration) {
            self.reading.lock().expect("test clock lock").monotonic = monotonic;
        }

        fn advance_monotonic(&self, elapsed: Duration) {
            let mut reading = self.reading.lock().expect("test clock lock");
            reading.monotonic = reading.monotonic.saturating_add(elapsed);
        }
    }

    impl crate::state::temporal::TemporalClock for TestFailureMemoryClock {
        fn read(&self) -> crate::state::temporal::TemporalClockReading {
            *self.reading.lock().expect("test clock lock")
        }
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
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
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
            destination.write(effects, contents)
        }
    }

    fn persistence_fixture(
        name: &str,
    ) -> (
        PathBuf,
        AppPaths,
        Arc<CountingFileBackend>,
        PersistenceCoordinator,
        AnchoredRecordDirectory,
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
        let directory = test_failure_memory_record_directory(&paths)
            .expect("open failure-memory record directory");
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("fixture time")
            .with_timezone(&chrono::Utc);
        let store = GuardianFailureMemoryStore::try_load_with_coordinator_directory_and_temporal(
            coordinator.clone(),
            directory.clone(),
            Arc::new(crate::state::temporal::BoundedTemporalPolicy::fixed(now)),
        )
        .expect("claim failure-memory persistence");
        (root, paths, backend, coordinator, directory, store)
    }

    #[test]
    fn failure_memory_entry_round_trips_strict_shape() {
        let entry = retry_entry("2026-06-15T10:00:00Z")
            .with_suppression_until("2026-06-15T10:30:00Z")
            .with_target_content_hash("sha256abc123");
        let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("snapshot");
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = FailureMemorySnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![entry]);
    }

    #[test]
    fn behavior_contract_exact_cooldown_and_future_skew_bound_all_admission_paths() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let store = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT).0;
        let exact_observed = (now + chrono::Duration::minutes(5)).to_rfc3339();
        let exact_until = (now + chrono::Duration::hours(24)).to_rfc3339();
        let exact_skew = temporal_entry("exact-skew", &exact_observed, None);
        store
            .construct_entry(exact_skew.clone())
            .expect("exact future skew constructs");
        store.record(exact_skew).expect("exact future skew admits");
        let exact_cooldown =
            temporal_entry("exact-cooldown", &now.to_rfc3339(), Some(&exact_until));
        store
            .construct_entry(exact_cooldown.clone())
            .expect("exact cooldown constructs");
        store
            .record(exact_cooldown.clone())
            .expect("exact cooldown admits");

        let encoded = FailureMemorySnapshot::new(vec![exact_cooldown.clone()])
            .expect("exact cooldown snapshot")
            .to_json()
            .expect("serialize exact cooldown");
        let decoded = FailureMemorySnapshot::from_json(&encoded).expect("decode exact cooldown");
        let round_trip = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT).0;
        round_trip
            .load_snapshot(decoded)
            .expect("max-valid cooldown loads after round trip");
        assert_eq!(round_trip.get(&exact_cooldown.key), Some(exact_cooldown));

        let offset_exact = temporal_entry(
            "offset-exact-cooldown",
            "2026-08-13T12:00:00+02:00",
            Some("2026-08-14T12:00:00+02:00"),
        );
        store
            .construct_entry(offset_exact)
            .expect("equivalent offset exact cooldown constructs");
        let offset_overlong = temporal_entry(
            "offset-overlong-cooldown",
            "2026-08-13T12:00:00+02:00",
            Some("2026-08-14T12:00:01+02:00"),
        );
        assert!(matches!(
            store.construct_entry(offset_overlong),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::SuppressionWindowOutOfBounds
            ))
        ));

        let future_plus_one =
            (now + chrono::Duration::minutes(5) + chrono::Duration::seconds(1)).to_rfc3339();
        let future = temporal_entry("future-plus-one", &future_plus_one, None);
        assert!(matches!(
            store.construct_entry(future.clone()),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::ObservedTimestampTooFarInFuture
            ))
        ));
        assert!(matches!(
            store.record(future),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::ObservedTimestampTooFarInFuture
            ))
        ));

        let cooldown_observed = now.to_rfc3339();
        let cooldown_plus_one =
            (now + chrono::Duration::hours(24) + chrono::Duration::seconds(1)).to_rfc3339();
        let overlong = temporal_entry(
            "cooldown-plus-one",
            &cooldown_observed,
            Some(&cooldown_plus_one),
        );
        assert!(matches!(
            store.construct_entry(overlong.clone()),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::SuppressionWindowOutOfBounds
            ))
        ));
        assert!(matches!(
            store.record(overlong),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::SuppressionWindowOutOfBounds
            ))
        ));
    }

    #[test]
    fn behavior_contract_far_future_capacity_is_quarantined_before_valid_admission() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, _) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let far_future = (now + chrono::Duration::days(365)).to_rfc3339();
        let snapshot = FailureMemorySnapshot::new(
            (0..super::DEFAULT_FAILURE_MEMORY_LIMIT)
                .map(|index| temporal_entry(&format!("future-{index}"), &far_future, None))
                .collect(),
        )
        .expect("structurally valid current-schema snapshot");

        store
            .load_snapshot(snapshot)
            .expect("temporal-invalid rows are quarantined");
        assert!(store.list().is_empty());
        assert_eq!(
            store.temporal_quarantine_count(),
            super::DEFAULT_FAILURE_MEMORY_LIMIT
        );

        let current = temporal_entry("current", &now.to_rfc3339(), None);
        store.record(current).expect("valid work retains capacity");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn behavior_contract_overlong_terminal_is_quarantined_but_malformed_is_fatal() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let valid = temporal_entry("valid-after-overlong", &now.to_rfc3339(), None);
        let overlong = reconciliation_entry_at(
            "overlong-load",
            now,
            now + chrono::Duration::hours(24) + chrono::Duration::seconds(1),
        );
        let encoded = FailureMemorySnapshot::new(vec![overlong, valid.clone()])
            .expect("overlong terminal remains structurally valid")
            .to_json()
            .expect("serialize mixed snapshot");
        let snapshot = FailureMemorySnapshot::from_json(&encoded)
            .expect("decode structurally valid mixed snapshot");
        let store = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT).0;
        store
            .load_snapshot(snapshot)
            .expect("overlong terminal is quarantined per row");
        assert_eq!(store.list(), vec![valid]);
        assert_eq!(store.temporal_quarantine_count(), 1);

        let mut malformed =
            reconciliation_entry_at("malformed-load", now, now + chrono::Duration::minutes(1));
        malformed.last_observed_at = "not-a-timestamp".to_string();
        assert!(matches!(
            FailureMemorySnapshot::new(vec![malformed]),
            Err(FailureMemoryLoadError::InvalidEntry(_))
        ));
    }

    #[test]
    fn behavior_contract_monotonic_time_ignores_wall_jumps_and_never_resurrects_expiry() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, clock) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let until = (now + chrono::Duration::minutes(30)).to_rfc3339();
        let entry = temporal_entry("clock-jump", &now.to_rfc3339(), Some(&until));
        store
            .record(entry.clone())
            .expect("record current cooldown");

        clock.set_wall(now - chrono::Duration::days(30));
        assert!(store.suppression_active(&entry));
        clock.set_wall(now + chrono::Duration::days(30));
        assert!(store.suppression_active(&entry));

        clock.advance_monotonic(Duration::from_secs(30 * 60));
        assert!(!store.suppression_active(&entry));
        clock.set_monotonic(Duration::ZERO);
        assert!(!store.suppression_active(&entry));
    }

    #[test]
    fn behavior_contract_temporal_policy_preserves_exact_intent_keys() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, _) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let until = (now + chrono::Duration::minutes(30)).to_rfc3339();
        let first = temporal_intent_entry("intent-a", &now.to_rfc3339(), &until);
        let second = temporal_intent_entry("intent-b", &now.to_rfc3339(), &until);
        store
            .record(
                store
                    .construct_entry(first.clone())
                    .expect("construct first"),
            )
            .expect("admit first");
        store
            .load_snapshot(
                FailureMemorySnapshot::new(vec![first.clone(), second.clone()])
                    .expect("same temporal rules load"),
            )
            .expect("load exact intent entries");

        assert_ne!(first.key, second.key);
        assert_eq!(store.get(&first.key), Some(first));
        assert_eq!(store.get(&second.key), Some(second));
    }

    #[test]
    fn behavior_contract_repair_reservation_uses_policy_now_not_caller_time() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, _) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let prior = persisted_repair_attempt_at(now);
        let prior_entry = GuardianFailureMemoryEntry::for_persisted_state_repair_terminal(
            PersistedStateRepairTerminal::from_attempt(
                prior,
                PersistedStateRepairTerminalOutcome::Refused,
            ),
        );
        store
            .record(prior_entry)
            .expect("seed active repair memory");

        let caller_shifted = persisted_repair_attempt_at(now + chrono::Duration::minutes(5));
        assert_eq!(
            store.reserve_persisted_state_repair(&caller_shifted).err(),
            Some(super::PersistedStateRepairReserveError::Suppressed)
        );
    }

    #[tokio::test]
    async fn behavior_contract_future_replacement_cannot_end_live_suppression_early() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, _) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let existing = reconciliation_entry_at("existing", now, now + chrono::Duration::minutes(3));
        let key = existing.key.clone();
        let reservation = store
            .reserve_reconciliation_attempt(key.clone())
            .expect("reserve initial reconciliation");
        store
            .record_reconciliation_terminal(existing, &reservation)
            .await
            .expect("record active reconciliation");
        drop(reservation);

        let replacement = reconciliation_entry_at(
            "replacement",
            now + chrono::Duration::minutes(5),
            now + chrono::Duration::minutes(35),
        );
        let replacement_reservation = store
            .reserve_reconciliation_attempt(key)
            .expect("same key can reserve its capacity slot");
        assert!(matches!(
            store.validate_reconciliation_terminal(&replacement, &replacement_reservation),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::ReconciliationTerminalMismatch
            ))
        ));
    }

    #[tokio::test]
    async fn behavior_contract_expired_pending_publication_can_be_acknowledged_and_pruned() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T10:00:00Z")
            .expect("test now")
            .with_timezone(&chrono::Utc);
        let (store, clock) = test_store_at(now, 1);
        let pending = pending_reconciliation_entry_at(now, now + chrono::Duration::minutes(1));
        let key = pending.key.clone();
        let reservation = store
            .reserve_reconciliation_attempt(key)
            .expect("reserve pending publication");
        store
            .record_reconciliation_terminal(pending.clone(), &reservation)
            .await
            .expect("persist pending publication");
        drop(reservation);

        clock.advance_monotonic(Duration::from_secs(2 * 60));
        assert_eq!(store.list_current(), vec![pending.clone()]);
        let terminal = pending.reconciliation_terminal().expect("pending terminal");
        let evidence = terminal
            .version_bundle_publication()
            .expect("pending publication")
            .evidence();
        let acknowledged_terminal = terminal
            .clone()
            .with_acknowledged_version_bundle_publication(evidence)
            .expect("acknowledge exact evidence");
        let mut acknowledged = pending.clone();
        acknowledged.reconciliation_terminal = Some(acknowledged_terminal);

        store
            .acknowledge_reconciliation_version_bundle_publication(&pending, acknowledged.clone())
            .await
            .expect("expired pending publication remains acknowledgeable");
        let mut snapshot_entries = store
            .snapshot()
            .expect("snapshot acknowledged carrier")
            .entries;
        snapshot_entries.push(temporal_entry(
            "ordinary-expired",
            &now.to_rfc3339(),
            Some(&(now + chrono::Duration::minutes(1)).to_rfc3339()),
        ));
        let snapshot = FailureMemorySnapshot::new(snapshot_entries)
            .expect("snapshot acknowledged and ordinary expired entries");
        let (reloaded, reloaded_clock) = test_store_at(now, 1);
        reloaded_clock.advance_monotonic(Duration::from_secs(2 * 60));
        reloaded
            .load_snapshot(snapshot)
            .expect("reload acknowledged carrier");
        assert_eq!(reloaded.list(), vec![acknowledged]);
        assert!(reloaded.list_current().is_empty());
        let replacement = reconciliation_entry_at(
            "capacity-reused",
            now + chrono::Duration::minutes(2),
            now + chrono::Duration::minutes(5),
        );
        let replacement_reservation = reloaded
            .reserve_reconciliation_attempt(replacement.key.clone())
            .expect("acknowledged expiry releases capacity");
        reloaded
            .record_reconciliation_terminal(replacement.clone(), &replacement_reservation)
            .await
            .expect("replacement prunes acknowledged expired carrier");
        assert_eq!(reloaded.list(), vec![replacement]);
    }

    #[tokio::test]
    async fn failure_memory_store_loads_a_valid_bounded_snapshot() {
        let root = test_root("bounded-valid-load");
        let paths = test_paths(&root);
        let path = super::failure_memory_path(&paths);
        fs::create_dir_all(path.parent().expect("failure-memory parent"))
            .expect("create failure-memory parent");
        let entry =
            retry_entry("2026-06-15T10:00:00Z").with_suppression_until("2026-06-15T10:30:00Z");
        let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("valid snapshot");
        fs::write(&path, snapshot.to_json().expect("encode valid snapshot"))
            .expect("write valid snapshot");

        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let directory = test_failure_memory_record_directory(&paths)
            .expect("open failure-memory record directory");
        let store = GuardianFailureMemoryStore::try_load_with_coordinator_directory_and_temporal(
            PersistenceCoordinator::current(),
            directory,
            Arc::new(crate::state::temporal::BoundedTemporalPolicy::fixed(now)),
        )
        .expect("load valid bounded snapshot");
        assert_eq!(store.get(&entry.key), Some(entry));

        store.close().await.expect("close valid loaded store");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retired_failure_memory_schemas_are_strict_invalid_and_preserved_byte_exact() {
        for retired_version in ["v4", "v5", "v6"] {
            let retired_schema = format!("axial.guardian.failure_memory.{retired_version}");
            let legacy = FAILURE_MEMORY_V7_FIXTURE.replacen(
                "axial.guardian.failure_memory.v7",
                &retired_schema,
                1,
            );
            assert!(matches!(
                FailureMemorySnapshot::from_json(&legacy),
                Err(FailureMemoryLoadError::InvalidSchema)
            ));

            let root = test_root(&format!("preserve-{retired_version}-schema"));
            let paths = test_paths(&root);
            let path = super::failure_memory_path(&paths);
            fs::create_dir_all(path.parent().expect("failure-memory parent"))
                .expect("create failure-memory parent");
            fs::write(&path, legacy.as_bytes()).expect("write retired failure-memory snapshot");

            assert!(matches!(
                GuardianFailureMemoryStore::try_load_from_paths(&paths),
                Err(FailureMemoryStoreError::Snapshot(
                    FailureMemoryLoadError::InvalidSchema
                ))
            ));
            assert_eq!(
                fs::read(&path).expect("retired failure-memory remains"),
                legacy.as_bytes()
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn failure_memory_store_reads_the_exact_snapshot_byte_bound() {
        let root = test_root("exact-snapshot-bound");
        let paths = test_paths(&root);
        let path = super::failure_memory_path(&paths);
        fs::create_dir_all(path.parent().expect("failure-memory parent"))
            .expect("create failure-memory parent");
        let file = fs::File::create(&path).expect("create exact-bound snapshot");
        file.set_len(super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES)
            .expect("size exact-bound snapshot");
        drop(file);

        assert!(matches!(
            GuardianFailureMemoryStore::try_load_from_paths(&paths),
            Err(FailureMemoryStoreError::Snapshot(
                FailureMemoryLoadError::Json(_)
            ))
        ));
        assert_eq!(
            fs::metadata(&path)
                .expect("exact-bound snapshot remains")
                .len(),
            super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failure_memory_store_rejects_an_oversized_regular_file_without_replacing_it() {
        let root = test_root("oversized-snapshot");
        let paths = test_paths(&root);
        let path = super::failure_memory_path(&paths);
        fs::create_dir_all(path.parent().expect("failure-memory parent"))
            .expect("create failure-memory parent");
        let file = fs::File::create(&path).expect("create oversized snapshot");
        file.set_len(super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES + 1)
            .expect("size oversized snapshot");
        drop(file);

        assert!(matches!(
            GuardianFailureMemoryStore::try_load_from_paths(&paths),
            Err(FailureMemoryStoreError::Snapshot(
                FailureMemoryLoadError::TooLarge
            ))
        ));
        assert_eq!(
            fs::metadata(&path)
                .expect("oversized snapshot remains")
                .len(),
            super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES + 1
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn failure_memory_store_rejects_a_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink-snapshot");
        let outside = test_root("symlink-snapshot-outside");
        let paths = test_paths(&root);
        let path = super::failure_memory_path(&paths);
        let outside_path = outside.join("outside-failure-memory.json");
        fs::create_dir_all(path.parent().expect("failure-memory parent"))
            .expect("create failure-memory parent");
        fs::write(&outside_path, FAILURE_MEMORY_V7_FIXTURE).expect("write outside snapshot");
        symlink(&outside_path, &path).expect("link failure-memory snapshot");

        assert!(matches!(
            GuardianFailureMemoryStore::try_load_from_paths(&paths),
            Err(FailureMemoryStoreError::Persistence(_))
        ));
        assert_eq!(
            fs::read_to_string(&outside_path).expect("outside snapshot remains readable"),
            FAILURE_MEMORY_V7_FIXTURE
        );
        assert!(
            fs::symlink_metadata(&path)
                .expect("failure-memory link remains")
                .file_type()
                .is_symlink()
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn failure_memory_encoder_accepts_the_exact_snapshot_bound_and_rejects_one_more_byte() {
        let mut snapshot = FailureMemorySnapshot {
            schema: String::new(),
            entries: Vec::new(),
        };
        let overhead = serde_json::to_vec(&snapshot)
            .expect("serialize empty snapshot")
            .len();
        assert_eq!(
            (overhead + super::FAILURE_MEMORY_SCHEMA.len()) as u64,
            super::FAILURE_MEMORY_SNAPSHOT_FIXED_BYTES
        );
        snapshot.schema = "x".repeat(super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES as usize - overhead);

        let encoded = super::encode_snapshot(snapshot.clone()).expect("accept exact size bound");
        assert_eq!(
            encoded.len() as u64,
            super::MAX_FAILURE_MEMORY_SNAPSHOT_BYTES
        );

        snapshot.schema.push('x');
        assert_eq!(
            super::encode_snapshot(snapshot)
                .expect_err("oversized snapshots must not be persisted")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn failure_memory_snapshot_rejects_an_entry_over_its_schema_budget() {
        let mut entry = retry_entry("2026-06-15T10:00:00Z");
        entry.target_content_hash =
            Some("x".repeat(super::MAX_FAILURE_MEMORY_ENTRY_BYTES as usize));

        assert!(matches!(
            FailureMemorySnapshot::new(vec![entry]),
            Err(FailureMemoryLoadError::TooLarge)
        ));
    }

    #[test]
    fn retired_failure_memory_schemas_are_rejected() {
        for schema in [
            "axial.guardian.failure_memory.v2",
            "axial.guardian.failure_memory.v3",
        ] {
            let value = serde_json::json!({"schema": schema, "entries": []});
            assert!(FailureMemorySnapshot::from_json(&value.to_string()).is_err());
        }
    }

    #[test]
    fn every_diagnosis_id_round_trips_through_strict_failure_memory_snapshot() {
        for diagnosis_id in DiagnosisId::ALL {
            let entry = GuardianFailureMemoryEntry::observed(
                diagnosis_id,
                GuardianDomain::Launch,
                classify_current_artifact(
                    CurrentArtifact::UnknownFilesystemPath,
                    diagnosis_id.as_str(),
                )
                .target,
                GuardianMode::Managed,
                Some("diagnosis_inventory"),
                "2026-06-15T10:00:00Z",
            );
            let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("snapshot");
            let encoded = snapshot.to_json().expect("serialize snapshot");
            let value = serde_json::from_str::<serde_json::Value>(&encoded).expect("snapshot json");

            assert_eq!(value["entries"][0]["diagnosis_id"], diagnosis_id.as_str());

            let decoded = FailureMemorySnapshot::from_json(&encoded).expect("strict snapshot");
            assert_eq!(decoded.schema, super::FAILURE_MEMORY_SCHEMA);
            assert_eq!(decoded.entries, vec![entry]);
        }
    }

    #[test]
    fn behavior_contract_v7_fixture_is_byte_stable_and_v6_is_retired() {
        let snapshot =
            FailureMemorySnapshot::from_json(FAILURE_MEMORY_V7_FIXTURE).expect("strict fixture");
        assert_eq!(
            super::FAILURE_MEMORY_SCHEMA,
            "axial.guardian.failure_memory.v7"
        );
        assert_eq!(snapshot.schema, "axial.guardian.failure_memory.v7");
        let retired_v6 = FAILURE_MEMORY_V7_FIXTURE.replacen(
            "axial.guardian.failure_memory.v7",
            "axial.guardian.failure_memory.v6",
            1,
        );
        assert!(matches!(
            FailureMemorySnapshot::from_json(&retired_v6),
            Err(FailureMemoryLoadError::InvalidSchema)
        ));
        let action_kinds = snapshot
            .entries
            .iter()
            .filter_map(|entry| entry.last_action_kind)
            .collect::<Vec<_>>();
        let expected_action_kinds = vec![
            GuardianActionKind::Repair,
            GuardianActionKind::Retry,
            GuardianActionKind::Strip,
            GuardianActionKind::Downgrade,
            GuardianActionKind::Fallback,
            GuardianActionKind::Repair,
            GuardianActionKind::Block,
        ];
        assert_eq!(action_kinds, expected_action_kinds);
        let action_outcomes = snapshot
            .entries
            .iter()
            .filter_map(|entry| entry.last_action_outcome)
            .collect::<std::collections::HashSet<_>>();
        for outcome in &action_outcomes {
            assert_fixture_action_outcome(*outcome);
        }
        assert_eq!(
            action_outcomes,
            std::collections::HashSet::from([
                FailureMemoryActionOutcome::Repaired,
                FailureMemoryActionOutcome::Retried,
                FailureMemoryActionOutcome::Blocked,
                FailureMemoryActionOutcome::Failed,
            ])
        );

        let terminals = snapshot
            .entries
            .iter()
            .filter_map(GuardianFailureMemoryEntry::reconciliation_terminal)
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 2, "fixture must exercise typed terminals");
        assert_eq!(
            terminals
                .iter()
                .map(|terminal| terminal.outcome())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationTerminalOutcome::Succeeded,
                ReconciliationTerminalOutcome::Failed,
            ]
        );
        assert_eq!(
            terminals
                .iter()
                .map(|terminal| terminal.rung())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationRung::RebuildComponent,
                ReconciliationRung::RepairArtifact,
            ]
        );
        assert_eq!(
            terminals
                .iter()
                .map(|terminal| terminal.component())
                .collect::<Vec<_>>(),
            vec![
                ReconciliationComponent::Runtime,
                ReconciliationComponent::Libraries,
            ]
        );
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            ..
        } = terminals[0].scope();
        assert_eq!(instance_id, "0123456789abcdef");
        assert_eq!(
            fingerprint.as_str(),
            "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef"
        );
        assert!(!terminals[0].quarantine_checkpoint().is_empty());
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            ..
        } = terminals[1].scope();
        assert_eq!(instance_id, "0123456789abcdef");
        assert_eq!(
            fingerprint.as_str(),
            "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef"
        );
        assert!(terminals[1].quarantine_checkpoint().is_empty());

        assert!(
            snapshot
                .entries
                .iter()
                .all(|entry| entry.persisted_state_repair_terminal().is_none())
        );

        let pretty = serde_json::to_string_pretty(&snapshot).expect("pretty fixture json");
        assert_eq!(format!("{pretty}\n"), FAILURE_MEMORY_V7_FIXTURE);

        let compact = snapshot.to_json().expect("compact fixture json");
        let decoded = FailureMemorySnapshot::from_json(&compact).expect("decode compact fixture");
        assert_eq!(
            decoded.to_json().expect("re-encode compact fixture"),
            compact
        );
    }

    fn assert_fixture_action_outcome(outcome: FailureMemoryActionOutcome) {
        match outcome {
            FailureMemoryActionOutcome::Repaired
            | FailureMemoryActionOutcome::Retried
            | FailureMemoryActionOutcome::Blocked
            | FailureMemoryActionOutcome::Failed => {}
        }
    }

    #[test]
    fn failure_memory_rejects_unknown_fields_and_unsafe_target_ids() {
        let value = serde_json::json!({
            "schema": super::FAILURE_MEMORY_SCHEMA,
            "entries": [{
                "key": "Launch:java_override_unavailable:State.FilesystemPath.target:Managed:intent",
                "diagnosis_id": "java_override_unavailable",
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
                "quarantine_checkpoint": { "records": [] },
                "suppression_until": null,
                "target_content_hash": null,
                "user_intent_hash": "intent",
                "reconciliation_terminal": null,
                "persisted_state_repair_terminal": null,
                "unexpected": true
            }]
        });

        assert!(FailureMemorySnapshot::from_json(&value.to_string()).is_err());

        let nested_unknown_field = serde_json::json!({
            "schema": super::FAILURE_MEMORY_SCHEMA,
            "entries": [{
                "key": "Launch:java_override_unavailable:State.FilesystemPath.target:Managed:intent",
                "diagnosis_id": "java_override_unavailable",
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
                "quarantine_checkpoint": { "records": [] },
                "suppression_until": null,
                "target_content_hash": null,
                "user_intent_hash": "intent",
                "reconciliation_terminal": null,
                "persisted_state_repair_terminal": null
            }]
        });
        assert!(FailureMemorySnapshot::from_json(&nested_unknown_field.to_string()).is_err());

        let unsafe_entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::JavaOverrideUnavailable,
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
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let store = GuardianFailureMemoryStore::at_for_test(now);
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
            DiagnosisId::ManagedRuntimeCorrupt,
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
        .with_quarantine_checkpoint(ReconciliationQuarantineCheckpoint::new(vec![
            super::super::contracts::ReconciliationQuarantineRecord::runtime("java-runtime-delta"),
        ]))
        .with_suppression_until("2026-06-15T10:20:00Z");

        assert_eq!(repair.repair_attempt_count, 1);
        assert!(!repair.quarantine_checkpoint.is_empty());
        assert_eq!(repair.ownership, OwnershipClass::LauncherManaged);
        assert!(repair.validate().is_ok());

        let repair_key = repair.key.clone();
        store.record(repair.clone()).expect("record repair");
        store.record(repair).expect("record repeated repair");
        let stored_repair = store.get(&repair_key).expect("stored repair");
        assert_eq!(stored_repair.occurrence_count, 2);
        assert_eq!(stored_repair.repair_attempt_count, 2);
    }

    #[tokio::test]
    async fn startup_install_retry_reconciliation_is_atomic_and_idempotent() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let (store, clock) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let initial = retry_entry("2026-06-15T10:00:00+00:00")
            .with_suppression_until("2026-06-15T10:05:00+00:00");
        let key = initial.key.clone();
        let replacement = retry_entry("2026-06-15T10:10:00+00:00")
            .with_suppression_until("2026-06-15T10:15:00+00:00");

        store
            .reconcile_install_guardian_retry(initial.clone())
            .await
            .expect("insert initial startup Retry");
        store
            .reconcile_install_guardian_retry(initial)
            .await
            .expect("exact startup Retry is a no-op");
        assert_eq!(store.get(&key).expect("initial Retry").occurrence_count, 1);

        clock.advance_monotonic(Duration::from_secs(10 * 60));
        store
            .reconcile_install_guardian_retry(replacement.clone())
            .await
            .expect("replace expired Retry with the new window");
        let merged = store.get(&key).expect("replacement Retry");
        assert_eq!(merged.first_observed_at, "2026-06-15T10:10:00+00:00");
        assert_eq!(merged.last_observed_at, "2026-06-15T10:10:00+00:00");
        assert_eq!(merged.occurrence_count, 1);
        assert_eq!(
            merged.suppression_until.as_deref(),
            Some("2026-06-15T10:15:00+00:00")
        );

        store
            .reconcile_install_guardian_retry(replacement)
            .await
            .expect("repeated merged Retry is a no-op");
        assert_eq!(store.get(&key).expect("idempotent merged Retry"), merged);
    }

    #[tokio::test]
    async fn startup_install_retry_batch_protects_every_active_key_atomically() {
        let retry = |target_id: &str| {
            GuardianFailureMemoryEntry::observed(
                DiagnosisId::DownloadUnavailable,
                GuardianDomain::Download,
                TargetDescriptor::new(
                    StabilizationSystem::Execution,
                    TargetKind::Artifact,
                    target_id,
                    OwnershipClass::LauncherManaged,
                ),
                GuardianMode::Managed,
                Some("install_provider"),
                "2026-06-15T10:00:00+00:00",
            )
            .with_action(
                GuardianActionKind::Retry,
                FailureMemoryActionOutcome::Retried,
            )
            .with_suppression_until("2026-06-15T10:05:00+00:00")
        };
        let entries = vec![
            retry("minecraft_client_1.21.5"),
            retry("loader_fabric_build"),
        ];

        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let undersized = test_store_at(now, 1).0;
        assert!(matches!(
            undersized
                .reconcile_install_guardian_retry_batch(entries.clone())
                .await,
            Err(FailureMemoryStoreError::CapacityExhausted)
        ));
        assert!(undersized.list().is_empty());

        let store = test_store_at(now, 2).0;
        store
            .reconcile_install_guardian_retry_batch(entries.clone())
            .await
            .expect("persist complete active Retry set");
        let restored = store.list();
        assert_eq!(restored.len(), 2);
        store
            .reconcile_install_guardian_retry_batch(entries)
            .await
            .expect("exact active Retry batch is a no-op");
        assert_eq!(store.list(), restored);
    }

    #[tokio::test]
    async fn startup_install_retry_reconciliation_rejects_drift_and_ambiguous_time() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let (store, clock) = test_store_at(now, super::DEFAULT_FAILURE_MEMORY_LIMIT);
        let initial = retry_entry("2026-06-15T10:00:00+00:00")
            .with_suppression_until("2026-06-15T10:05:00+00:00");
        let key = initial.key.clone();
        store
            .reconcile_install_guardian_retry(initial.clone())
            .await
            .expect("insert initial startup Retry");
        let _ = clock;

        let replacement = || {
            retry_entry("2026-06-15T10:10:00+00:00")
                .with_suppression_until("2026-06-15T10:15:00+00:00")
        };
        let mut ownership_drift = replacement();
        ownership_drift.ownership = OwnershipClass::LauncherManaged;
        ownership_drift.target.ownership = OwnershipClass::LauncherManaged;
        let mut target_drift = replacement();
        target_drift.target.id = "different_target".to_string();
        let mut domain_drift = replacement();
        domain_drift.domain = GuardianDomain::Runtime;
        let mut mode_drift = replacement();
        mode_drift.mode = GuardianMode::Custom;
        let mut diagnosis_drift = replacement();
        diagnosis_drift.diagnosis_id = DiagnosisId::OutOfMemory;
        let action_drift = replacement().with_action(
            GuardianActionKind::Retry,
            FailureMemoryActionOutcome::Blocked,
        );
        let mut intent_drift = replacement();
        intent_drift.user_intent_hash = Some("different_intent".to_string());
        let overlapping = retry_entry("2026-06-15T10:04:00+00:00")
            .with_suppression_until("2026-06-15T10:09:00+00:00");
        let reverse = retry_entry("2026-06-15T09:50:00+00:00")
            .with_suppression_until("2026-06-15T09:55:00+00:00");
        let mut ambiguous = replacement();
        ambiguous.last_observed_at = "2026-06-15T10:11:00+00:00".to_string();
        let expired = retry_entry("2026-06-15T10:10:00+00:00")
            .with_suppression_until("2026-06-15T10:10:00+00:00");
        let wrong_deadline = retry_entry("2026-06-15T10:10:00+00:00")
            .with_suppression_until("2026-06-15T10:16:00+00:00");
        let noncanonical_utc =
            retry_entry("2026-06-15T10:10:00Z").with_suppression_until("2026-06-15T10:15:00Z");
        let offset_time = retry_entry("2026-06-15T11:10:00+01:00")
            .with_suppression_until("2026-06-15T11:15:00+01:00");

        for rejected in [
            ownership_drift,
            target_drift,
            domain_drift,
            mode_drift,
            diagnosis_drift,
            action_drift,
            intent_drift,
            overlapping,
            reverse,
            ambiguous,
            expired,
            wrong_deadline,
            noncanonical_utc,
            offset_time,
        ] {
            let result = store.reconcile_install_guardian_retry(rejected).await;
            assert!(
                matches!(
                    result,
                    Err(FailureMemoryStoreError::Validation(_))
                        | Err(FailureMemoryStoreError::Expired)
                ),
                "unexpected retry reconciliation result: {result:?}"
            );
            assert_eq!(store.get(&key), Some(initial.clone()));
        }
    }

    #[test]
    fn changed_target_hash_reset_shape_is_explicit() {
        let entry = retry_entry("2026-06-15T12:00:00Z").with_target_content_hash("sha256_old123");

        assert!(!entry.target_content_changed("sha256_old123"));
        assert!(entry.target_content_changed("sha256_new456"));
    }

    #[test]
    fn invalid_remote_rules_failure_cooldown_uses_external_provider_target() {
        let target = classify_current_artifact(
            CurrentArtifact::ExternalPerformanceRules,
            "performance_rules_remote_source",
        )
        .target;
        let entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::PerformanceRulesInvalid,
            GuardianDomain::Performance,
            target,
            GuardianMode::Managed,
            Some("rules_manifest_v1"),
            "2026-06-15T13:00:00Z",
        )
        .with_action(
            GuardianActionKind::RecordOnly,
            FailureMemoryActionOutcome::Failed,
        )
        .with_suppression_until("2026-06-15T13:05:00Z");

        assert_eq!(entry.ownership, OwnershipClass::ExternalProviderDerived);
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn failure_memory_store_bounds_retention_to_recent_entries() {
        let store = GuardianFailureMemoryStore::with_max_entries(2);
        for (diagnosis, observed_at) in [
            (DiagnosisId::LaunchPrepareFailed, "2026-06-15T10:00:00Z"),
            (DiagnosisId::StartupFailedUnknown, "2026-06-15T10:01:00Z"),
            (DiagnosisId::OutOfMemory, "2026-06-15T10:02:00Z"),
        ] {
            let entry = GuardianFailureMemoryEntry::observed(
                diagnosis,
                GuardianDomain::Launch,
                classify_current_artifact(
                    CurrentArtifact::UnknownFilesystemPath,
                    diagnosis.as_str(),
                )
                .target,
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
                .all(|entry| entry.diagnosis_id != DiagnosisId::LaunchPrepareFailed)
        );
    }

    #[tokio::test]
    async fn active_reconciliation_memory_matches_the_journal_capacity() {
        assert_eq!(
            super::DEFAULT_FAILURE_MEMORY_LIMIT,
            DEFAULT_OPERATION_JOURNAL_LIMIT
        );
        let store = GuardianFailureMemoryStore::new();

        for index in 0..DEFAULT_OPERATION_JOURNAL_LIMIT {
            let entry = active_reconciliation_entry(index);
            let reservation = store
                .reserve_reconciliation_attempt(entry.key.clone())
                .expect("reserve reconciliation memory slot");
            store
                .record_reconciliation_terminal(entry, &reservation)
                .await
                .expect("journal-capacity terminal fits failure memory");
        }

        assert_eq!(store.list().len(), DEFAULT_OPERATION_JOURNAL_LIMIT);
    }

    #[test]
    fn exact_reconciliation_validation_is_non_mutating_and_reservation_bound() {
        let store = GuardianFailureMemoryStore::with_max_entries(1);
        let entry = active_reconciliation_entry(0);
        let key = entry.key.clone();
        let reservation = store
            .reserve_reconciliation_attempt(key.clone())
            .expect("reserve exact reconciliation slot");

        store
            .validate_reconciliation_terminal(&entry, &reservation)
            .expect("exact terminal and reservation validate");
        assert!(store.get(&key).is_none());

        let foreign_store = GuardianFailureMemoryStore::with_max_entries(1);
        let foreign_reservation = foreign_store
            .reserve_reconciliation_attempt(key)
            .expect("reserve foreign reconciliation slot");
        assert!(matches!(
            store.validate_reconciliation_terminal(&entry, &foreign_reservation),
            Err(FailureMemoryStoreError::Validation(
                super::FailureMemoryValidationError::MemoryKeyMismatch
            ))
        ));
    }

    #[test]
    fn concurrent_reconciliation_reservations_cannot_overbook_capacity() {
        let store = Arc::new(GuardianFailureMemoryStore::with_max_entries(1));
        let barrier = Arc::new(Barrier::new(3));
        let keys = [
            active_reconciliation_entry(0).key,
            active_reconciliation_entry(1).key,
        ];
        let reservations = std::thread::scope(|scope| {
            let handles = keys.map(|key| {
                let store = store.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    store.reserve_reconciliation_attempt(key)
                })
            });
            barrier.wait();
            handles.map(|handle| handle.join().expect("reservation worker"))
        });

        assert_eq!(
            reservations.iter().filter(|result| result.is_ok()).count(),
            1
        );
        assert_eq!(
            reservations
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        Err(ReconciliationAttemptReserveError::CapacityExhausted)
                    )
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn active_same_key_reconciliation_reservation_reuses_its_capacity_slot() {
        let store = GuardianFailureMemoryStore::with_max_entries(1);
        let active = active_reconciliation_entry(0);
        let active_key = active.key.clone();
        let initial = store
            .reserve_reconciliation_attempt(active_key.clone())
            .expect("reserve initial reconciliation slot");
        store
            .record_reconciliation_terminal(active, &initial)
            .await
            .expect("record active reconciliation terminal");
        drop(initial);

        let replacement = store
            .reserve_reconciliation_attempt(active_key)
            .expect("active key reuses its occupied slot");
        assert!(matches!(
            store.reserve_reconciliation_attempt(active_reconciliation_entry(1).key),
            Err(ReconciliationAttemptReserveError::CapacityExhausted)
        ));
        drop(replacement);
    }

    #[test]
    fn dropped_reconciliation_reservation_releases_capacity() {
        let store = GuardianFailureMemoryStore::with_max_entries(1);
        let first = store
            .reserve_reconciliation_attempt(active_reconciliation_entry(0).key)
            .expect("reserve only slot");
        let second_key = active_reconciliation_entry(1).key;
        assert!(matches!(
            store.reserve_reconciliation_attempt(second_key.clone()),
            Err(ReconciliationAttemptReserveError::CapacityExhausted)
        ));

        drop(first);
        let second = store
            .reserve_reconciliation_attempt(second_key)
            .expect("released slot can be reserved");
        drop(second);
    }

    #[tokio::test]
    async fn ordinary_prunable_memory_does_not_consume_reconciliation_capacity() {
        let store = GuardianFailureMemoryStore::with_max_entries(1);
        let ordinary = retry_entry("2026-06-15T10:00:00Z");
        let ordinary_key = ordinary.key.clone();
        store.record(ordinary).expect("seed ordinary memory");
        let terminal = active_reconciliation_entry(0);
        let terminal_key = terminal.key.clone();
        let reservation = store
            .reserve_reconciliation_attempt(terminal_key.clone())
            .expect("ordinary memory leaves reconciliation capacity available");
        store
            .record_reconciliation_terminal(terminal, &reservation)
            .await
            .expect("active terminal displaces ordinary memory");

        assert!(store.get(&ordinary_key).is_none());
        assert!(store.get(&terminal_key).is_some());
    }

    #[test]
    fn rejected_active_snapshot_preserves_visible_memory() {
        let store = GuardianFailureMemoryStore::with_max_entries(2);
        let prior = retry_entry("2026-06-15T10:00:00Z");
        store.record(prior.clone()).expect("seed visible memory");
        let before = store.list();
        let snapshot =
            FailureMemorySnapshot::new((0..3).map(active_reconciliation_entry).collect())
                .expect("globally bounded active snapshot");

        assert!(matches!(
            store.load_snapshot(snapshot),
            Err(FailureMemoryLoadError::TooManyEntries)
        ));
        assert_eq!(store.list(), before);
        assert_eq!(store.get(&prior.key), Some(prior));
    }

    #[tokio::test]
    async fn failure_memory_burst_coalesces_and_reloads_the_latest_cumulative_snapshot() {
        let (root, paths, backend, _coordinator, _directory, store) =
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
        let (root, paths, backend, _coordinator, _directory, store) =
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
    async fn failed_startup_install_retry_persistence_stays_hidden_until_retry() {
        let (root, paths, backend, _coordinator, _directory, store) =
            persistence_fixture("startup-install-retry-hidden");
        let entry = retry_entry("2026-06-15T10:00:00+00:00")
            .with_suppression_until("2026-06-15T10:05:00+00:00");
        let key = entry.key.clone();
        backend.fail_next();

        assert!(matches!(
            store.reconcile_install_guardian_retry(entry.clone()).await,
            Err(FailureMemoryStoreError::Persistence(_))
        ));
        assert!(store.get(&key).is_none());
        assert!(
            store
                .records
                .read()
                .expect(super::FAILURE_MEMORY_LOCK_INVARIANT)
                .retry_candidate
                .is_some()
        );

        store.retry().await.expect("persist hidden Retry candidate");
        assert_eq!(store.get(&key), Some(entry.clone()));
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);

        store
            .reconcile_install_guardian_retry(entry.clone())
            .await
            .expect("persisted Retry replay is a no-op");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        let encoded = fs::read_to_string(super::failure_memory_path(&paths))
            .expect("read persisted startup Retry");
        let snapshot = FailureMemorySnapshot::from_json(&encoded).expect("decode startup Retry");
        assert_eq!(snapshot.entries, vec![entry]);

        store.close().await.expect("close startup Retry store");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn close_retries_latest_failed_snapshot_and_releases_owner() {
        let (root, _paths, backend, coordinator, directory, store) =
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

        let reloaded = GuardianFailureMemoryStore::try_load_from_directory_with_coordinator(
            directory,
            coordinator,
        )
        .expect("closed owner is released");
        let entry = reloaded.get(&key).expect("latest snapshot reloads");
        assert_eq!(entry.occurrence_count, 2);
        assert_eq!(entry.last_observed_at, "2026-06-15T10:01:00Z");
        reloaded.close().await.expect("close reloaded store");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failure_memory_snapshot_path_has_one_exclusive_owner() {
        let (root, _paths, _backend, coordinator, directory, first) =
            persistence_fixture("duplicate-owner");
        let second = GuardianFailureMemoryStore::try_load_from_directory_with_coordinator(
            directory,
            coordinator,
        );

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
        let (root, _paths, backend, _coordinator, _directory, store) =
            persistence_fixture("closed-acceptance");
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
        let (root, _paths, backend, _coordinator, _directory, store) =
            persistence_fixture("poisoned-lock");
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
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T10:00:00Z")
            .expect("test time")
            .with_timezone(&chrono::Utc);
        let directory = test_failure_memory_record_directory(&paths)
            .expect("open failure-memory record directory");
        let store = GuardianFailureMemoryStore::try_load_with_coordinator_directory_and_temporal(
            PersistenceCoordinator::current(),
            directory,
            Arc::new(crate::state::temporal::BoundedTemporalPolicy::fixed(now)),
        )
        .expect("load Guardian failure-memory persistence");
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
        let reloaded = GuardianFailureMemoryStore::at_for_test(now);
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

    fn test_store_at(
        now: chrono::DateTime<chrono::Utc>,
        max_entries: usize,
    ) -> (GuardianFailureMemoryStore, Arc<TestFailureMemoryClock>) {
        let clock = Arc::new(TestFailureMemoryClock::new(now));
        let temporal = Arc::new(crate::state::temporal::BoundedTemporalPolicy::new(
            clock.clone(),
        ));
        (
            GuardianFailureMemoryStore::with_max_entries_and_clock(max_entries, temporal, None),
            clock,
        )
    }

    fn temporal_entry(
        target_id: &str,
        observed_at: &str,
        suppression_until: Option<&str>,
    ) -> GuardianFailureMemoryEntry {
        let mut entry = GuardianFailureMemoryEntry::observed(
            DiagnosisId::StartupFailedUnknown,
            GuardianDomain::Launch,
            TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Instance,
                target_id,
                OwnershipClass::LauncherManaged,
            ),
            GuardianMode::Managed,
            Some("intent-a"),
            observed_at,
        );
        if let Some(suppression_until) = suppression_until {
            entry = entry.with_suppression_until(suppression_until);
        }
        entry
    }

    fn temporal_intent_entry(
        intent: &str,
        observed_at: &str,
        suppression_until: &str,
    ) -> GuardianFailureMemoryEntry {
        GuardianFailureMemoryEntry::observed(
            DiagnosisId::StartupFailedUnknown,
            GuardianDomain::Launch,
            TargetDescriptor::new(
                StabilizationSystem::Guardian,
                TargetKind::Instance,
                "same-target",
                OwnershipClass::LauncherManaged,
            ),
            GuardianMode::Managed,
            Some(intent),
            observed_at,
        )
        .with_suppression_until(suppression_until)
    }

    fn persisted_repair_attempt_at(
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> PersistedStateRepairAttempt {
        PersistedStateRepairAttempt::new(
            PersistedStateRecordStore::BenchmarkSuiteDriver,
            "benchmark-suite-driver-0000000000000001",
            RestartStableRecordIdentity::from_digest([1; 32]),
            GuardianMode::Managed,
            observed_at.to_rfc3339(),
        )
    }

    fn reconciliation_entry_at(
        operation: &str,
        observed_at: chrono::DateTime<chrono::Utc>,
        suppression_until: chrono::DateTime<chrono::Utc>,
    ) -> GuardianFailureMemoryEntry {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "same-reconciliation-target",
            OwnershipClass::LauncherManaged,
        );
        let attempt = ReconciliationAttempt::new(
            OperationId::deterministic_test(format!("temporal-{operation}")),
            DiagnosisId::LauncherManagedArtifactCorrupt,
            GuardianDomain::Library,
            ReconciliationRung::RepairArtifact,
            ReconciliationScope::RegisteredInstance {
                instance_id: "0123456789abcdef".to_string(),
                fingerprint: ReconciliationIncarnationFingerprint::from_digest(
                    "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef",
                ),
                inventory_fingerprint: ReconciliationInventoryFingerprint::from_digest(
                    "sha256.11111111.22222222.33333333.44444444.55555555.66666666.77777777.88888888",
                ),
                activation_contract_id: axial_minecraft::ManagedInstallActivationContractId::parse(
                    "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
                )
                .expect("canonical activation contract"),
            },
            ReconciliationComponent::Libraries,
            target,
            GuardianMode::Managed,
            OwnershipClass::LauncherManaged,
            observed_at.to_rfc3339(),
            suppression_until.to_rfc3339(),
            ReconciliationLineage::Initial,
        );
        crate::state::reconciliation_memory_entry(ReconciliationTerminal::from_attempt(
            attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
        ))
        .expect("valid temporal reconciliation entry")
    }

    fn pending_reconciliation_entry_at(
        observed_at: chrono::DateTime<chrono::Utc>,
        suppression_until: chrono::DateTime<chrono::Utc>,
    ) -> GuardianFailureMemoryEntry {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "version-bundle",
            OwnershipClass::LauncherManaged,
        );
        let attempt = ReconciliationAttempt::new(
            OperationId::deterministic_test("temporal-pending-publication"),
            DiagnosisId::LauncherManagedArtifactCorrupt,
            GuardianDomain::Library,
            ReconciliationRung::RebuildComponent,
            ReconciliationScope::RegisteredInstance {
                instance_id: "0123456789abcdef".to_string(),
                fingerprint: ReconciliationIncarnationFingerprint::from_digest(
                    "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef",
                ),
                inventory_fingerprint: ReconciliationInventoryFingerprint::from_digest(
                    "sha256.11111111.22222222.33333333.44444444.55555555.66666666.77777777.88888888",
                ),
                activation_contract_id: axial_minecraft::ManagedInstallActivationContractId::parse(
                    "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
                )
                .expect("canonical activation contract"),
            },
            ReconciliationComponent::VersionBundle,
            target,
            GuardianMode::Managed,
            OwnershipClass::LauncherManaged,
            observed_at.to_rfc3339(),
            suppression_until.to_rfc3339(),
            ReconciliationLineage::Predecessor {
                operation_id: OperationId::deterministic_test("temporal-pending-predecessor"),
            },
        );
        let evidence = axial_minecraft::ManagedInstallPublicationEvidenceId::parse(
            "managed-install-v1.T7ghN0PBffcxr4Rg08bVvTPOl9fRcUh9qyNnWZtd93c.Xsu8KmJnT7So_J1WS8rcqA.X-fR4EpDTc2mbfPpfNOFiA.JGoynsQN9LfT8e7hWyX1fknDskeaM7xQCAbFGATbD-I._FMcn_pUsOarv_sNtTJousevn4S1SMqttV6yiYdONOY",
        )
        .expect("canonical publication evidence");
        let terminal = ReconciliationTerminal::from_attempt(
            attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
        )
        .with_version_bundle_publication(evidence, ReconciliationVersionBundleOutcome::RolledBack);
        crate::state::reconciliation_memory_entry(terminal)
            .expect("valid pending publication memory")
    }

    fn retry_entry(observed_at: &str) -> GuardianFailureMemoryEntry {
        GuardianFailureMemoryEntry::observed(
            DiagnosisId::StartupFailedUnknown,
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

    fn active_reconciliation_entry(index: usize) -> GuardianFailureMemoryEntry {
        let observed_at = chrono::Utc::now().fixed_offset();
        let suppression_until = observed_at + chrono::Duration::minutes(15);
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            format!("library-artifact-{index}"),
            OwnershipClass::LauncherManaged,
        );
        let attempt = ReconciliationAttempt::new(
            OperationId::deterministic_test(format!("reconciliation-capacity-{index}")),
            DiagnosisId::LauncherManagedArtifactCorrupt,
            GuardianDomain::Library,
            ReconciliationRung::RepairArtifact,
            ReconciliationScope::RegisteredInstance {
                instance_id: "0123456789abcdef".to_string(),
                fingerprint: ReconciliationIncarnationFingerprint::from_digest(
                    "sha256.aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee.ffffffff.01234567.89abcdef",
                ),
                inventory_fingerprint: ReconciliationInventoryFingerprint::from_digest(
                    "sha256.11111111.22222222.33333333.44444444.55555555.66666666.77777777.88888888",
                ),
                activation_contract_id: axial_minecraft::ManagedInstallActivationContractId::parse(
                    "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
                )
                .expect("canonical test activation contract"),
            },
            ReconciliationComponent::Libraries,
            target,
            GuardianMode::Managed,
            OwnershipClass::LauncherManaged,
            observed_at.to_rfc3339(),
            suppression_until.to_rfc3339(),
            ReconciliationLineage::Initial,
        );
        reconciliation_memory_entry(ReconciliationTerminal::from_attempt(
            attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
        ))
        .expect("valid reconciliation memory")
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
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
