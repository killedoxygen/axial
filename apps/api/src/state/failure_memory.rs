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
use super::ownership::{CurrentArtifact, classify_current_artifact};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
use axial_config::AppPaths;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub const FAILURE_MEMORY_SCHEMA: &str = "axial.guardian.failure_memory.v4";
pub const DEFAULT_FAILURE_MEMORY_LIMIT: usize = RECONCILIATION_EVIDENCE_CAPACITY;
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
        let diagnosis = diagnosis_id.as_str();
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
        } = reconciliation_scope;
        let scope = format!(
            "registered.{instance_id}.{}.{}",
            fingerprint.as_str(),
            inventory_fingerprint.as_str()
        );
        let base = Self::for_observation(domain, diagnosis_id, target, mode, None);
        Self(format!(
            "{}:rung.{:?}:component.{:?}:scope.{scope}",
            base.as_str(),
            rung,
            component,
        ))
    }

    pub(super) fn for_persisted_state_repair(attempt: &PersistedStateRepairAttempt) -> Self {
        Self(format!(
            "State:{}:Managed:{:?}:{}:{}",
            DiagnosisId::PersistedStateSchemaInvalid.as_str(),
            attempt.store(),
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
    InvalidReconciliationTerminal,
    ReconciliationTerminalMismatch,
    InvalidPersistedStateRepairTerminal,
    PersistedStateRepairTerminalMismatch,
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
}

impl FailureMemoryStoreError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Snapshot(_) => "snapshot",
            Self::Persistence(_) => "persistence",
            Self::CapacityExhausted => "capacity_exhausted",
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
    records: Arc<RwLock<FailureMemoryRecords>>,
    attempts: Arc<Mutex<BTreeSet<String>>>,
    max_entries: usize,
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
        Self {
            records: Arc::new(RwLock::new(FailureMemoryRecords::default())),
            attempts: Arc::new(Mutex::new(BTreeSet::new())),
            max_entries: max_entries.clamp(1, DEFAULT_FAILURE_MEMORY_LIMIT),
            persistence: None,
        }
    }

    pub fn try_load_from_paths(paths: &AppPaths) -> Result<Self, FailureMemoryStoreError> {
        Self::try_load_from_paths_with_coordinator(paths, PersistenceCoordinator::global())
    }

    pub(crate) fn try_load_from_paths_with_coordinator(
        paths: &AppPaths,
        coordinator: PersistenceCoordinator,
    ) -> Result<Self, FailureMemoryStoreError> {
        let storage_path = failure_memory_path(paths);
        let store = Self::with_max_entries_and_persistence(
            DEFAULT_FAILURE_MEMORY_LIMIT,
            Some(FailureMemoryPersistence::claim(&storage_path, coordinator)?),
        );

        match fs::read_to_string(&storage_path) {
            Ok(data) => store.load_snapshot(FailureMemorySnapshot::from_json(&data)?)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(FailureMemoryStoreError::Persistence(error)),
        }

        Ok(store)
    }

    fn with_max_entries_and_persistence(
        max_entries: usize,
        persistence: Option<FailureMemoryPersistence>,
    ) -> Self {
        Self {
            records: Arc::new(RwLock::new(FailureMemoryRecords::default())),
            attempts: Arc::new(Mutex::new(BTreeSet::new())),
            max_entries: max_entries.clamp(1, DEFAULT_FAILURE_MEMORY_LIMIT),
            persistence,
        }
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
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        if records.retry_candidate.is_some() || records.critical_pending {
            return Err(FailureMemoryStoreError::Persistence(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Guardian failure-memory persistence requires retry",
            )));
        }
        entry.validate()?;
        let protected_key = (entry.reconciliation_terminal().is_some()
            || entry.persisted_state_repair_terminal().is_some())
        .then(|| entry.key.as_str().to_string());
        let mut candidate = records.visible.clone();
        apply(&mut candidate, entry);
        if !prune_records(&mut candidate, self.max_entries, protected_key.as_deref()) {
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
        let pending =
            self.record_with(entry, apply_reconciliation_record, WriteUrgency::Immediate)?;
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
        let pending =
            self.record_with(entry, apply_reconciliation_record, WriteUrgency::Immediate)?;
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
        entry.validate()?;
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
                stored != entry && !persisted_state_repair_entry_can_be_superseded(stored, entry)
            })
        {
            return Err(FailureMemoryValidationError::PersistedStateRepairTerminalMismatch.into());
        }
        let mut candidate = records.visible.clone();
        apply_reconciliation_record(&mut candidate, entry.clone());
        if !prune_records(&mut candidate, self.max_entries, Some(entry.key.as_str())) {
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
        entry.validate()?;
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
            && !reconciliation_entry_can_be_superseded(stored, entry)
        {
            return Err(FailureMemoryValidationError::ReconciliationTerminalMismatch.into());
        }

        let mut candidate = records.visible.clone();
        apply_reconciliation_record(&mut candidate, entry.clone());
        if !prune_records(&mut candidate, self.max_entries, Some(entry.key.as_str())) {
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
                .filter(|entry| active_durable_terminal(entry))
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
        let observed_at = DateTime::parse_from_rfc3339(attempt.observed_at())
            .map_err(|_| PersistedStateRepairReserveError::InvalidAttempt)?;
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
            .and_then(GuardianFailureMemoryEntry::persisted_state_repair_terminal)
            .and_then(|terminal| DateTime::parse_from_rfc3339(terminal.suppression_until()).ok())
            .is_some_and(|until| until > observed_at)
        {
            return Err(PersistedStateRepairReserveError::Suppressed);
        }
        let mut occupied_keys = attempts.clone();
        occupied_keys.extend(
            records
                .visible
                .values()
                .filter(|entry| active_durable_terminal(entry))
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

    pub(super) async fn settle_reconciliation_pending(
        &self,
    ) -> Result<(), FailureMemoryStoreError> {
        let (critical_pending, retry_pending) = {
            let records = self.records.read().expect(FAILURE_MEMORY_LOCK_INVARIANT);
            (records.critical_pending, records.retry_candidate.is_some())
        };
        if !critical_pending {
            return Ok(());
        }
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

    pub fn snapshot(&self) -> Result<FailureMemorySnapshot, FailureMemoryLoadError> {
        FailureMemorySnapshot::new(self.list())
    }

    pub fn load_snapshot(
        &self,
        snapshot: FailureMemorySnapshot,
    ) -> Result<(), FailureMemoryLoadError> {
        snapshot.validate()?;
        let mut candidate = BTreeMap::new();
        for entry in snapshot.entries {
            candidate.insert(entry.key.as_str().to_string(), entry);
        }
        if !prune_records(&mut candidate, self.max_entries, None) {
            return Err(FailureMemoryLoadError::TooManyEntries);
        }
        let mut records = self.records.write().expect(FAILURE_MEMORY_LOCK_INVARIANT);
        records.visible = candidate;
        records.visible_revision = 0;
        records.retry_candidate = None;
        records.critical_pending = false;
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

impl Default for GuardianFailureMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn prune_records(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    max_entries: usize,
    protected_key: Option<&str>,
) -> bool {
    if records.len() <= max_entries {
        return true;
    }

    let mut ordered = records
        .values()
        .filter(|entry| {
            protected_key != Some(entry.key.as_str()) && !active_durable_terminal(entry)
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

fn active_durable_terminal(entry: &GuardianFailureMemoryEntry) -> bool {
    entry
        .reconciliation_terminal()
        .and_then(|terminal| DateTime::parse_from_rfc3339(terminal.suppression_until()).ok())
        .is_some_and(|until| until > chrono::Utc::now())
        || entry
            .persisted_state_repair_terminal()
            .and_then(|terminal| DateTime::parse_from_rfc3339(terminal.suppression_until()).ok())
            .is_some_and(|until| until > chrono::Utc::now())
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

fn apply_reconciliation_record(
    records: &mut BTreeMap<String, GuardianFailureMemoryEntry>,
    entry: GuardianFailureMemoryEntry,
) {
    records.insert(entry.key.as_str().to_string(), entry);
}

fn reconciliation_entry_can_be_superseded(
    existing: &GuardianFailureMemoryEntry,
    replacement: &GuardianFailureMemoryEntry,
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
    let Ok(existing_until) = DateTime::parse_from_rfc3339(existing_terminal.suppression_until())
    else {
        return false;
    };
    let Ok(replacement_observed) = DateTime::parse_from_rfc3339(replacement_terminal.observed_at())
    else {
        return false;
    };
    existing_until <= replacement_observed
}

fn persisted_state_repair_entry_can_be_superseded(
    existing: &GuardianFailureMemoryEntry,
    replacement: &GuardianFailureMemoryEntry,
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
    let Ok(existing_until) = DateTime::parse_from_rfc3339(existing_terminal.suppression_until())
    else {
        return false;
    };
    let Ok(replacement_observed) =
        DateTime::parse_from_rfc3339(replacement_terminal.attempt().observed_at())
    else {
        return false;
    };
    existing_until <= replacement_observed
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
        FailureMemoryActionOutcome, FailureMemoryLoadError, FailureMemorySnapshot,
        FailureMemoryStoreError, GuardianFailureMemoryEntry, GuardianFailureMemoryStore,
        ReconciliationAttemptReserveError,
    };
    use crate::execution::file::{FileWriteRequest, write_file_atomically};
    use crate::execution::persistence::{AtomicWriteBackend, PersistenceCoordinator};
    use crate::guardian::{DiagnosisId, GuardianActionKind, GuardianDomain, GuardianMode};
    use crate::state::contracts::{
        OperationId, OwnershipClass, PersistedStateRepairTerminalOutcome, ReconciliationAttempt,
        ReconciliationComponent, ReconciliationIncarnationFingerprint,
        ReconciliationInventoryFingerprint, ReconciliationLineage,
        ReconciliationQuarantineCheckpoint, ReconciliationRung, ReconciliationScope,
        ReconciliationTerminal, ReconciliationTerminalOutcome, StabilizationSystem,
        TargetDescriptor, TargetKind,
    };
    use crate::state::journals::DEFAULT_OPERATION_JOURNAL_LIMIT;
    use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
    use crate::state::reconciliation_memory_entry;
    use axial_config::AppPaths;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const FAILURE_MEMORY_V4_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/guardian/failure-memory-v4.json"
    ));

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
            .with_target_content_hash("sha256abc123");
        let snapshot = FailureMemorySnapshot::new(vec![entry.clone()]).expect("snapshot");
        let encoded = snapshot.to_json().expect("serialize snapshot");
        let decoded = FailureMemorySnapshot::from_json(&encoded).expect("deserialize snapshot");

        assert_eq!(decoded.entries, vec![entry]);
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
    fn checked_in_failure_memory_v4_fixture_is_byte_stable() {
        let snapshot =
            FailureMemorySnapshot::from_json(FAILURE_MEMORY_V4_FIXTURE).expect("strict fixture");
        assert_eq!(
            super::FAILURE_MEMORY_SCHEMA,
            "axial.guardian.failure_memory.v4"
        );
        assert_eq!(snapshot.schema, "axial.guardian.failure_memory.v4");
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
            GuardianActionKind::Quarantine,
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

        let persisted_state_terminals = snapshot
            .entries
            .iter()
            .filter_map(GuardianFailureMemoryEntry::persisted_state_repair_terminal)
            .collect::<Vec<_>>();
        assert_eq!(persisted_state_terminals.len(), 1);
        assert_eq!(
            persisted_state_terminals[0].outcome(),
            PersistedStateRepairTerminalOutcome::Refused
        );

        let pretty = serde_json::to_string_pretty(&snapshot).expect("pretty fixture json");
        assert_eq!(format!("{pretty}\n"), FAILURE_MEMORY_V4_FIXTURE);

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
        let store = GuardianFailureMemoryStore::try_load_from_paths(&paths)
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
            OperationId::new(format!("reconciliation-capacity-{index}")),
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
