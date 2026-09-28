//! State system contracts.
//!
//! The existing `state` module still owns runtime `AppState` and current stores.
//! This submodule exposes the durable vocabulary for journals, ownership,
//! snapshots, and persistence boundaries used by the target systems.

use crate::guardian::{
    DiagnosisId, GuardianActionKind, GuardianDomain, GuardianFactId, GuardianMode,
};
use crate::observability::evidence_text_looks_sensitive;
use axial_minecraft::{
    ManagedInstallActivationContractId,
    known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ENTRIES},
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::{Uuid, Variant, Version};

pub(crate) const RECONCILIATION_EVIDENCE_CAPACITY: usize = 128;
pub const RECONCILIATION_QUARANTINE_CAPACITY: usize = 8;
pub(crate) const MAX_DURABLE_GUARDIAN_FACT_IDS: usize = 64;
pub(crate) const MAX_DURABLE_GUARDIAN_DIAGNOSES: usize = 32;
pub(super) const PERSISTED_STATE_REPAIR_SUPPRESSION_HOURS: i64 = 24;
pub(super) const PERSISTED_STATE_REPAIR_MAX_ATTEMPTS_PER_STABLE_KEY_PER_SUPPRESSION_WINDOW: usize =
    1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistedStateRecordStore {
    BenchmarkSuiteDriver,
}

impl PersistedStateRecordStore {
    #[cfg(test)]
    pub(crate) const ALL: &'static [Self] = &[Self::BenchmarkSuiteDriver];

    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::BenchmarkSuiteDriver => "BenchmarkSuiteDriver",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct RestartStableRecordIdentity(String);

impl RestartStableRecordIdentity {
    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut value = String::with_capacity(78);
        value.push_str("sha256");
        for chunk in digest.chunks_exact(4) {
            value.push('.');
            for byte in chunk {
                value.push(HEX[(byte >> 4) as usize] as char);
                value.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
        debug_assert!(valid_restart_stable_record_identity(&value));
        Self(value)
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RestartStableRecordIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if valid_restart_stable_record_identity(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "restart-stable record identity must be a canonical SHA-256 digest",
            ))
        }
    }
}

fn valid_restart_stable_record_identity(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256.") else {
        return false;
    };
    let mut segments = digest.split('.');
    (0..8).all(|_| {
        segments.next().is_some_and(|segment| {
            segment.len() == 8
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    }) && segments.next().is_none()
}

const OPERATION_ID_PREFIX: &str = "op-";
const OPERATION_ID_ENCODED_LEN: usize = 39;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationId(Uuid);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidOperationId;

impl fmt::Display for InvalidOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid operation id")
    }
}

impl std::error::Error for InvalidOperationId {}

impl OperationId {
    pub(crate) fn mint() -> Self {
        Self(Uuid::new_v4())
    }

    #[cfg(test)]
    pub(crate) fn deterministic_test(value: impl AsRef<[u8]>) -> Self {
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(value.as_ref());
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(OPERATION_ID_PREFIX)?;
        fmt::Display::fmt(&self.0.hyphenated(), formatter)
    }
}

impl FromStr for OperationId {
    type Err = InvalidOperationId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != OPERATION_ID_ENCODED_LEN {
            return Err(InvalidOperationId);
        }
        let encoded = value
            .strip_prefix(OPERATION_ID_PREFIX)
            .ok_or(InvalidOperationId)?;
        let uuid = Uuid::parse_str(encoded).map_err(|_| InvalidOperationId)?;
        let operation_id = Self(uuid);
        if uuid.get_version() != Some(Version::Random)
            || uuid.get_variant() != Variant::RFC4122
            || operation_id.to_string() != value
        {
            return Err(InvalidOperationId);
        }
        Ok(operation_id)
    }
}

impl TryFrom<&str> for OperationId {
    type Error = InvalidOperationId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl Serialize for OperationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OperationIdVisitor;

        impl<'de> serde::de::Visitor<'de> for OperationIdVisitor {
            type Value = OperationId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical lowercase UUID v4 operation id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value.parse().map_err(|_| E::custom("invalid operation id"))
            }
        }

        deserializer.deserialize_str(OperationIdVisitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReconciliationRung {
    RepairArtifact,
    RebuildComponent,
}

const RECONCILIATION_MAX_ATTEMPTS_PER_SUPPRESSION_WINDOW: usize = 1;

impl ReconciliationRung {
    pub const ALL: &'static [Self] = &[Self::RepairArtifact, Self::RebuildComponent];

    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::RepairArtifact => "RepairArtifact",
            Self::RebuildComponent => "RebuildComponent",
        }
    }

    pub(crate) const fn max_attempts_per_suppression_window(self) -> usize {
        match self {
            Self::RepairArtifact | Self::RebuildComponent => {
                RECONCILIATION_MAX_ATTEMPTS_PER_SUPPRESSION_WINDOW
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReconciliationComponent {
    VersionBundle,
    Libraries,
    Assets,
    Runtime,
}

impl ReconciliationComponent {
    pub const ALL: &'static [Self] = &[
        Self::VersionBundle,
        Self::Libraries,
        Self::Assets,
        Self::Runtime,
    ];

    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::VersionBundle => "VersionBundle",
            Self::Libraries => "Libraries",
            Self::Assets => "Assets",
            Self::Runtime => "Runtime",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationIncarnationFingerprint(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationInventoryFingerprint(String);

impl ReconciliationIncarnationFingerprint {
    pub(super) fn from_digest(digest: impl Into<String>) -> Self {
        Self(digest.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ReconciliationInventoryFingerprint {
    pub(super) fn from_digest(digest: impl Into<String>) -> Self {
        Self(digest.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ReconciliationScope {
    RegisteredInstance {
        instance_id: String,
        fingerprint: ReconciliationIncarnationFingerprint,
        inventory_fingerprint: ReconciliationInventoryFingerprint,
        activation_contract_id: ManagedInstallActivationContractId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ReconciliationLineage {
    Initial,
    Predecessor { operation_id: OperationId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReconciliationTerminalOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ReconciliationQuarantineRecord {
    Artifact { target: TargetDescriptor },
    RuntimeComponent { component_id: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationQuarantineCheckpoint {
    records: Vec<ReconciliationQuarantineRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationAttempt {
    operation_id: OperationId,
    diagnosis_id: DiagnosisId,
    domain: GuardianDomain,
    rung: ReconciliationRung,
    scope: ReconciliationScope,
    component: ReconciliationComponent,
    target: TargetDescriptor,
    mode: GuardianMode,
    ownership: OwnershipClass,
    observed_at: String,
    suppression_until: String,
    lineage: ReconciliationLineage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationTerminal {
    attempt: ReconciliationAttempt,
    outcome: ReconciliationTerminalOutcome,
    quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    version_bundle_publication: Option<ReconciliationVersionBundlePublication>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationVersionBundleOutcome {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationVersionBundlePublication {
    evidence: axial_minecraft::ManagedInstallPublicationEvidenceId,
    outcome: ReconciliationVersionBundleOutcome,
    acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) enum PersistedStateRepairTerminalOutcome {
    Quarantined,
    Refused,
    AppliedUnverified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedStateRepairAttempt {
    operation_id: OperationId,
    store: PersistedStateRecordStore,
    record_id: String,
    physical_identity: RestartStableRecordIdentity,
    target: TargetDescriptor,
    mode: GuardianMode,
    observed_at: String,
    suppression_until: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedStateRepairTerminal {
    attempt: PersistedStateRepairAttempt,
    outcome: PersistedStateRepairTerminalOutcome,
}

impl PersistedStateRepairAttempt {
    pub(super) fn new(
        store: PersistedStateRecordStore,
        record_id: impl Into<String>,
        physical_identity: RestartStableRecordIdentity,
        mode: GuardianMode,
        observed_at: impl Into<String>,
    ) -> Self {
        let record_id = record_id.into();
        let observed_at = observed_at.into();
        let suppression_until = chrono::DateTime::parse_from_rfc3339(&observed_at)
            .ok()
            .and_then(|observed| {
                observed.checked_add_signed(chrono::Duration::hours(
                    PERSISTED_STATE_REPAIR_SUPPRESSION_HOURS,
                ))
            })
            .map(|until| until.to_rfc3339())
            .unwrap_or_default();
        Self {
            operation_id: OperationId::mint(),
            store,
            target: persisted_state_repair_record_target(store, &record_id),
            record_id,
            physical_identity,
            mode,
            observed_at,
            suppression_until,
        }
    }

    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    pub(super) fn journal_id(&self) -> JournalId {
        JournalId::new(format!("journal-{}", self.operation_id))
    }

    pub(crate) const fn store(&self) -> PersistedStateRecordStore {
        self.store
    }

    pub(crate) fn record_id(&self) -> &str {
        &self.record_id
    }

    pub(crate) fn physical_identity(&self) -> &RestartStableRecordIdentity {
        &self.physical_identity
    }

    pub(crate) fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub(crate) fn observed_at(&self) -> &str {
        &self.observed_at
    }

    pub(crate) fn suppression_until(&self) -> &str {
        &self.suppression_until
    }

    pub(super) fn validate(&self) -> Result<(), PersistedStateRepairValidationError> {
        if !safe_reconciliation_token(&self.record_id, 128)
            || !super::benchmark_suite_drivers::is_safe_driver_id(&self.record_id)
        {
            return Err(PersistedStateRepairValidationError::UnsafeRecordId);
        }
        if self.mode != GuardianMode::Managed {
            return Err(PersistedStateRepairValidationError::InvalidMode);
        }
        if self.target != persisted_state_repair_record_target(self.store, &self.record_id) {
            return Err(PersistedStateRepairValidationError::UnsafeTarget);
        }
        let observed_at = chrono::DateTime::parse_from_rfc3339(&self.observed_at)
            .map_err(|_| PersistedStateRepairValidationError::InvalidWindow)?;
        let suppression_until = chrono::DateTime::parse_from_rfc3339(&self.suppression_until)
            .map_err(|_| PersistedStateRepairValidationError::InvalidWindow)?;
        if suppression_until <= observed_at {
            return Err(PersistedStateRepairValidationError::InvalidWindow);
        }
        Ok(())
    }
}

pub(super) fn persisted_state_repair_quarantine_suffix(
    attempt: &PersistedStateRepairAttempt,
) -> [u8; 16] {
    *attempt.operation_id.0.as_bytes()
}

fn persisted_state_repair_record_target(
    store: PersistedStateRecordStore,
    record_id: &str,
) -> TargetDescriptor {
    super::persisted_state_load::persisted_state_record_target(store, record_id)
}

impl PersistedStateRepairTerminal {
    pub(super) fn from_attempt(
        attempt: PersistedStateRepairAttempt,
        outcome: PersistedStateRepairTerminalOutcome,
    ) -> Self {
        Self { attempt, outcome }
    }

    pub(crate) fn attempt(&self) -> &PersistedStateRepairAttempt {
        &self.attempt
    }

    pub(crate) const fn outcome(&self) -> PersistedStateRepairTerminalOutcome {
        self.outcome
    }

    pub(crate) fn operation_id(&self) -> &OperationId {
        self.attempt.operation_id()
    }

    pub(crate) fn suppression_until(&self) -> &str {
        self.attempt.suppression_until()
    }

    pub(super) fn validate(&self) -> Result<(), PersistedStateRepairValidationError> {
        self.attempt.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PersistedStateRepairValidationError {
    UnsafeRecordId,
    UnsafeTarget,
    InvalidMode,
    InvalidWindow,
}

impl ReconciliationQuarantineRecord {
    pub(crate) fn artifact(target: TargetDescriptor) -> Self {
        Self::Artifact { target }
    }

    pub(crate) fn runtime(component_id: impl Into<String>) -> Self {
        Self::RuntimeComponent {
            component_id: component_id.into(),
        }
    }

    pub fn artifact_target(&self) -> Option<&TargetDescriptor> {
        match self {
            Self::Artifact { target } => Some(target),
            Self::RuntimeComponent { .. } => None,
        }
    }

    pub fn runtime_component_id(&self) -> Option<&str> {
        match self {
            Self::RuntimeComponent { component_id } => Some(component_id),
            Self::Artifact { .. } => None,
        }
    }
}

impl ReconciliationQuarantineCheckpoint {
    pub(crate) fn new(records: Vec<ReconciliationQuarantineRecord>) -> Self {
        Self { records }
    }

    pub fn records(&self) -> &[ReconciliationQuarantineRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub(super) fn validate_bounded(&self) -> Result<(), ReconciliationTerminalValidationError> {
        if self.records.len() > RECONCILIATION_QUARANTINE_CAPACITY {
            return Err(ReconciliationTerminalValidationError::TooManyQuarantines);
        }
        for (index, record) in self.records.iter().enumerate() {
            if self.records[..index].contains(record) {
                return Err(ReconciliationTerminalValidationError::UnsafeTarget);
            }
            match record {
                ReconciliationQuarantineRecord::Artifact { target }
                    if target.ownership == OwnershipClass::LauncherManaged
                        && target.system == StabilizationSystem::Execution
                        && !target.id.trim().is_empty()
                        && !target.id.contains(['/', '\\']) => {}
                ReconciliationQuarantineRecord::RuntimeComponent { component_id }
                    if axial_minecraft::runtime::is_known_runtime_component(component_id) => {}
                _ => return Err(ReconciliationTerminalValidationError::UnsafeTarget),
            }
        }
        Ok(())
    }

    fn validate_for(
        &self,
        attempt: &ReconciliationAttempt,
    ) -> Result<(), ReconciliationTerminalValidationError> {
        self.validate_bounded()?;
        for record in &self.records {
            match record {
                ReconciliationQuarantineRecord::Artifact { target } => {
                    let expected = TargetDescriptor::new(
                        StabilizationSystem::Execution,
                        attempt.target().kind,
                        format!("quarantine-{}", attempt.target().id),
                        attempt.ownership(),
                    );
                    if !matches!(
                        attempt.component(),
                        ReconciliationComponent::VersionBundle
                            | ReconciliationComponent::Libraries
                            | ReconciliationComponent::Assets
                    ) || target != &expected
                        || target.ownership != OwnershipClass::LauncherManaged
                    {
                        return Err(ReconciliationTerminalValidationError::UnsafeTarget);
                    }
                }
                ReconciliationQuarantineRecord::RuntimeComponent { component_id } => {
                    if !axial_minecraft::runtime::is_known_runtime_component(component_id)
                        || !matches!(attempt.component(), ReconciliationComponent::Runtime)
                        || attempt.target().id != *component_id
                    {
                        return Err(ReconciliationTerminalValidationError::UnsafeTarget);
                    }
                }
            }
        }
        Ok(())
    }
}

impl ReconciliationAttempt {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        operation_id: OperationId,
        diagnosis_id: DiagnosisId,
        domain: GuardianDomain,
        rung: ReconciliationRung,
        scope: ReconciliationScope,
        component: ReconciliationComponent,
        target: TargetDescriptor,
        mode: GuardianMode,
        ownership: OwnershipClass,
        observed_at: impl Into<String>,
        suppression_until: impl Into<String>,
        lineage: ReconciliationLineage,
    ) -> Self {
        Self {
            operation_id,
            diagnosis_id,
            domain,
            rung,
            scope,
            component,
            target,
            mode,
            ownership,
            observed_at: observed_at.into(),
            suppression_until: suppression_until.into(),
            lineage,
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    pub const fn diagnosis_id(&self) -> DiagnosisId {
        self.diagnosis_id
    }

    pub const fn domain(&self) -> GuardianDomain {
        self.domain
    }

    pub const fn rung(&self) -> ReconciliationRung {
        self.rung
    }

    pub fn scope(&self) -> &ReconciliationScope {
        &self.scope
    }

    pub const fn component(&self) -> ReconciliationComponent {
        self.component
    }

    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub const fn mode(&self) -> GuardianMode {
        self.mode
    }

    pub const fn ownership(&self) -> OwnershipClass {
        self.ownership
    }

    pub fn observed_at(&self) -> &str {
        &self.observed_at
    }

    pub fn suppression_until(&self) -> &str {
        &self.suppression_until
    }

    pub fn lineage(&self) -> &ReconciliationLineage {
        &self.lineage
    }

    pub(super) fn validate(&self) -> Result<(), ReconciliationTerminalValidationError> {
        if self.ownership != OwnershipClass::LauncherManaged {
            return Err(ReconciliationTerminalValidationError::UnsafeOwnership);
        }
        if self.target.ownership != self.ownership
            || self.target.id.trim().is_empty()
            || self.target.id.contains(['/', '\\'])
        {
            return Err(ReconciliationTerminalValidationError::UnsafeTarget);
        }
        if self.mode != GuardianMode::Managed {
            return Err(ReconciliationTerminalValidationError::NonManagedMode);
        }
        let observed_at = chrono::DateTime::parse_from_rfc3339(&self.observed_at)
            .map_err(|_| ReconciliationTerminalValidationError::InvalidWindow)?;
        let suppression_until = chrono::DateTime::parse_from_rfc3339(&self.suppression_until)
            .map_err(|_| ReconciliationTerminalValidationError::InvalidWindow)?;
        if suppression_until <= observed_at {
            return Err(ReconciliationTerminalValidationError::InvalidWindow);
        }
        let ReconciliationScope::RegisteredInstance {
            instance_id,
            fingerprint,
            inventory_fingerprint,
            activation_contract_id: _,
        } = &self.scope;
        if !axial_config::is_canonical_instance_id(instance_id) {
            return Err(ReconciliationTerminalValidationError::UnsafeInstanceId);
        }
        if !valid_reconciliation_fingerprint(fingerprint.as_str()) {
            return Err(ReconciliationTerminalValidationError::UnsafeFingerprint);
        }
        if !valid_reconciliation_fingerprint(inventory_fingerprint.as_str()) {
            return Err(ReconciliationTerminalValidationError::UnsafeInventoryFingerprint);
        }
        match (&self.lineage, self.rung) {
            (ReconciliationLineage::Initial, ReconciliationRung::RepairArtifact) => {}
            (
                ReconciliationLineage::Predecessor { operation_id },
                ReconciliationRung::RebuildComponent,
            ) if operation_id != &self.operation_id => {}
            _ => return Err(ReconciliationTerminalValidationError::InvalidLineage),
        }
        match self.component {
            ReconciliationComponent::VersionBundle
                if matches!(self.target.kind, TargetKind::Artifact | TargetKind::Version) => {}
            ReconciliationComponent::Libraries | ReconciliationComponent::Assets
                if self.target.kind == TargetKind::Artifact => {}
            ReconciliationComponent::Runtime if self.target.kind == TargetKind::Runtime => {}
            _ => return Err(ReconciliationTerminalValidationError::ImpossibleComponent),
        }
        Ok(())
    }
}

impl ReconciliationTerminal {
    pub(super) fn from_attempt(
        attempt: ReconciliationAttempt,
        outcome: ReconciliationTerminalOutcome,
        quarantine_checkpoint: ReconciliationQuarantineCheckpoint,
    ) -> Self {
        Self {
            attempt,
            outcome,
            quarantine_checkpoint,
            version_bundle_publication: None,
        }
    }

    pub(super) fn with_version_bundle_publication(
        mut self,
        evidence: axial_minecraft::ManagedInstallPublicationEvidenceId,
        outcome: ReconciliationVersionBundleOutcome,
    ) -> Self {
        self.version_bundle_publication = Some(ReconciliationVersionBundlePublication {
            evidence,
            outcome,
            acknowledged: false,
        });
        self
    }

    pub(super) fn with_acknowledged_version_bundle_publication(
        mut self,
        expected: &axial_minecraft::ManagedInstallPublicationEvidenceId,
    ) -> Result<Self, ReconciliationTerminalValidationError> {
        let publication = self
            .version_bundle_publication
            .as_mut()
            .filter(|publication| publication.evidence() == expected)
            .ok_or(ReconciliationTerminalValidationError::InvalidPublication)?;
        publication.acknowledged = true;
        self.validate()?;
        Ok(self)
    }

    pub fn attempt(&self) -> &ReconciliationAttempt {
        &self.attempt
    }

    pub fn operation_id(&self) -> &OperationId {
        self.attempt.operation_id()
    }

    pub const fn diagnosis_id(&self) -> DiagnosisId {
        self.attempt.diagnosis_id()
    }

    pub const fn domain(&self) -> GuardianDomain {
        self.attempt.domain()
    }

    pub const fn rung(&self) -> ReconciliationRung {
        self.attempt.rung()
    }

    pub fn scope(&self) -> &ReconciliationScope {
        self.attempt.scope()
    }

    pub const fn component(&self) -> ReconciliationComponent {
        self.attempt.component()
    }

    pub fn target(&self) -> &TargetDescriptor {
        self.attempt.target()
    }

    pub const fn mode(&self) -> GuardianMode {
        self.attempt.mode()
    }

    pub const fn ownership(&self) -> OwnershipClass {
        self.attempt.ownership()
    }

    pub fn observed_at(&self) -> &str {
        self.attempt.observed_at()
    }

    pub fn suppression_until(&self) -> &str {
        self.attempt.suppression_until()
    }

    pub const fn outcome(&self) -> ReconciliationTerminalOutcome {
        self.outcome
    }

    pub fn quarantine_checkpoint(&self) -> &ReconciliationQuarantineCheckpoint {
        &self.quarantine_checkpoint
    }

    pub fn version_bundle_publication(&self) -> Option<&ReconciliationVersionBundlePublication> {
        self.version_bundle_publication.as_ref()
    }

    pub(super) fn validate(&self) -> Result<(), ReconciliationTerminalValidationError> {
        self.attempt.validate()?;
        self.quarantine_checkpoint.validate_for(&self.attempt)?;
        match (
            self.version_bundle_publication.as_ref(),
            self.attempt.rung(),
            self.attempt.component(),
            self.outcome,
        ) {
            (
                Some(ReconciliationVersionBundlePublication {
                    outcome: ReconciliationVersionBundleOutcome::Committed,
                    ..
                }),
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::VersionBundle,
                ReconciliationTerminalOutcome::Succeeded,
            )
            | (
                Some(_),
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::VersionBundle,
                ReconciliationTerminalOutcome::Failed,
            )
            | (
                None,
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::VersionBundle,
                ReconciliationTerminalOutcome::Failed,
            )
            | (None, _, ReconciliationComponent::Libraries, _)
            | (None, _, ReconciliationComponent::Assets, _)
            | (None, _, ReconciliationComponent::Runtime, _)
            | (
                None,
                ReconciliationRung::RepairArtifact,
                ReconciliationComponent::VersionBundle,
                _,
            ) => {}
            _ => return Err(ReconciliationTerminalValidationError::InvalidPublication),
        }
        Ok(())
    }
}

impl ReconciliationVersionBundlePublication {
    pub fn evidence(&self) -> &axial_minecraft::ManagedInstallPublicationEvidenceId {
        &self.evidence
    }

    pub const fn outcome(&self) -> ReconciliationVersionBundleOutcome {
        self.outcome
    }

    pub const fn is_pending(&self) -> bool {
        !self.acknowledged
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReconciliationTerminalValidationError {
    UnsafeInstanceId,
    UnsafeFingerprint,
    UnsafeInventoryFingerprint,
    UnsafeOwnership,
    UnsafeTarget,
    TooManyQuarantines,
    InvalidLineage,
    NonManagedMode,
    InvalidWindow,
    ImpossibleComponent,
    InvalidPublication,
}

fn safe_reconciliation_token(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= max_chars
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '+')
        })
}

pub(super) fn valid_reconciliation_fingerprint(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256.") else {
        return false;
    };
    let segments = digest.split('.').collect::<Vec<_>>();
    segments.len() == 8
        && segments.iter().all(|segment| {
            segment.len() == 8
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct JournalId(pub String);

impl JournalId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum StabilizationSystem {
    Application,
    Execution,
    Guardian,
    Performance,
    Observability,
    State,
    Interface,
}

impl StabilizationSystem {
    pub const ALL: &'static [Self] = &[
        Self::Application,
        Self::Execution,
        Self::Guardian,
        Self::Performance,
        Self::Observability,
        Self::State,
        Self::Interface,
    ];

    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::Execution => "Execution",
            Self::Guardian => "Guardian",
            Self::Performance => "Performance",
            Self::Observability => "Observability",
            Self::State => "State",
            Self::Interface => "Interface",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum CommandKind {
    LaunchInstance,
    InstallVersion,
    ModifyInstanceContent,
    RepairInstance,
    RepairPersistedState,
    ApplyPerformancePlan,
    RefreshPerformanceRules,
    ValidateInstance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OwnershipClass {
    LauncherManaged,
    CompositionManaged,
    UserOwned,
    ExternalProviderDerived,
    Unknown,
}

impl OwnershipClass {
    pub const ALL: &'static [Self] = &[
        Self::LauncherManaged,
        Self::CompositionManaged,
        Self::UserOwned,
        Self::ExternalProviderDerived,
        Self::Unknown,
    ];

    #[cfg(test)]
    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::LauncherManaged => "LauncherManaged",
            Self::CompositionManaged => "CompositionManaged",
            Self::UserOwned => "UserOwned",
            Self::ExternalProviderDerived => "ExternalProviderDerived",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetDescriptor {
    pub system: StabilizationSystem,
    pub kind: TargetKind,
    pub id: String,
    pub ownership: OwnershipClass,
}

impl TargetDescriptor {
    pub fn new(
        system: StabilizationSystem,
        kind: TargetKind,
        id: impl Into<String>,
        ownership: OwnershipClass,
    ) -> Self {
        let id = id.into();
        Self {
            system,
            kind,
            id: sanitize_target_id(&id, "target"),
            ownership,
        }
    }
}

pub(crate) fn sanitize_target_id(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() || evidence_text_looks_sensitive(value) || has_windows_drive_prefix(value) {
        return fallback.to_string();
    }

    let mut sanitized = String::with_capacity(value.len().min(96));
    for ch in value.chars().take(96) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized.to_string()
    }
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum TargetKind {
    Instance,
    Version,
    Artifact,
    Runtime,
    Session,
    Account,
    Config,
    PerformanceComposition,
    FilesystemPath,
    NetworkResource,
}

impl TargetKind {
    pub const ALL: &'static [Self] = &[
        Self::Instance,
        Self::Version,
        Self::Artifact,
        Self::Runtime,
        Self::Session,
        Self::Account,
        Self::Config,
        Self::PerformanceComposition,
        Self::FilesystemPath,
        Self::NetworkResource,
    ];

    pub(crate) const fn failure_memory_id(self) -> &'static str {
        match self {
            Self::Instance => "Instance",
            Self::Version => "Version",
            Self::Artifact => "Artifact",
            Self::Runtime => "Runtime",
            Self::Session => "Session",
            Self::Account => "Account",
            Self::Config => "Config",
            Self::PerformanceComposition => "PerformanceComposition",
            Self::FilesystemPath => "FilesystemPath",
            Self::NetworkResource => "NetworkResource",
        }
    }
}

macro_rules! operation_phases {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
        pub enum OperationPhase {
            $($variant),+
        }

        impl OperationPhase {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

operation_phases! {
    Startup,
    Planning,
    Validating,
    Downloading,
    Installing,
    Preparing,
    Launching,
    Running,
    Repairing,
    RollingBack,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OperationStatus {
    Requested,
    Planned,
    Running,
    WaitingForUser,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceOperationAction {
    Install,
    Remove,
    Rollback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationIntent {
    pub instance_id: String,
    pub requested_action: PerformanceOperationAction,
    pub action: PerformanceOperationAction,
    pub base_target_id: String,
    pub rollback: RollbackState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "snake_case", deny_unknown_fields)]
pub enum PerformancePreparedProof {
    InstallPlan {
        graph_sha512: String,
        artifact_count: u64,
        aggregate_bytes: u64,
    },
    RemoveCurrent {
        graph_sha512: String,
        artifact_count: u64,
    },
    ManagedStateAbsent {},
    RollbackSnapshot {
        snapshot_id: String,
        target: PerformanceRollbackTarget,
        artifact_count: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceRollbackTarget {
    ManagedStateAbsent,
    ManagedComposition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationPrepared {
    pub result_target_id: String,
    pub proof: PerformancePreparedProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PerformanceOperationTerminal {
    Succeeded {
        prepared: PerformanceOperationPrepared,
        changed_target: bool,
        rollback: RollbackState,
    },
    FailedBeforeEffect {
        error: String,
    },
    FailedAfterEffect {
        prepared: PerformanceOperationPrepared,
        changed_target: bool,
        rollback: RollbackState,
        error: String,
    },
    AbandonedBeforeEffect {},
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum PerformanceOperationPhase {
    Accepted {},
    Planning {},
    Prepared {
        prepared: PerformanceOperationPrepared,
    },
    EffectStarted {
        prepared: PerformanceOperationPrepared,
    },
    AppliedUnverified {
        prepared: PerformanceOperationPrepared,
        error: String,
    },
    TerminalIntent {
        terminal: PerformanceOperationTerminal,
    },
    Terminal {
        terminal: PerformanceOperationTerminal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceOperationLifecycle {
    pub intent: PerformanceOperationIntent,
    pub phase: PerformanceOperationPhase,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationIntent {
    Generic {},
    Performance(PerformanceOperationLifecycle),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableGuardianEvidenceError {
    Empty,
    TooManyFacts,
    DuplicateFact,
    TooManyDiagnoses,
    DuplicateDiagnosis,
    TerminalDiagnosisMissing,
    InvalidInstallTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableGuardianEvidence {
    operation_id: OperationId,
    fact_ids: Vec<GuardianFactId>,
    diagnosis_ids: Vec<DiagnosisId>,
    install_terminal: Option<GuardianInstallTerminalEvidence>,
}

impl DurableGuardianEvidence {
    pub(crate) fn new(
        operation_id: OperationId,
        fact_ids: Vec<GuardianFactId>,
        diagnosis_ids: Vec<DiagnosisId>,
        install_terminal: Option<GuardianInstallTerminalEvidence>,
    ) -> Result<Self, DurableGuardianEvidenceError> {
        if fact_ids.is_empty() && diagnosis_ids.is_empty() && install_terminal.is_none() {
            return Err(DurableGuardianEvidenceError::Empty);
        }
        if fact_ids.len() > MAX_DURABLE_GUARDIAN_FACT_IDS {
            return Err(DurableGuardianEvidenceError::TooManyFacts);
        }
        if contains_duplicate(&fact_ids) {
            return Err(DurableGuardianEvidenceError::DuplicateFact);
        }
        if diagnosis_ids.len() > MAX_DURABLE_GUARDIAN_DIAGNOSES {
            return Err(DurableGuardianEvidenceError::TooManyDiagnoses);
        }
        if contains_duplicate(&diagnosis_ids) {
            return Err(DurableGuardianEvidenceError::DuplicateDiagnosis);
        }
        if let Some(terminal) = &install_terminal {
            terminal
                .validate()
                .map_err(|_| DurableGuardianEvidenceError::InvalidInstallTerminal)?;
            if !diagnosis_ids.contains(&terminal.diagnosis_id) {
                return Err(DurableGuardianEvidenceError::TerminalDiagnosisMissing);
            }
        }
        Ok(Self {
            operation_id,
            fact_ids,
            diagnosis_ids,
            install_terminal,
        })
    }

    pub(crate) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    pub(crate) fn fact_ids(&self) -> &[GuardianFactId] {
        &self.fact_ids
    }

    pub(crate) fn diagnosis_ids(&self) -> &[DiagnosisId] {
        &self.diagnosis_ids
    }

    pub(crate) fn install_terminal(&self) -> Option<&GuardianInstallTerminalEvidence> {
        self.install_terminal.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardianInstallTerminalEvidenceError {
    Shape,
    Target,
    Window,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianInstallTerminalEvidence {
    diagnosis_id: DiagnosisId,
    action: GuardianActionKind,
    memory: Option<GuardianInstallMemoryEvidence>,
}

impl GuardianInstallTerminalEvidence {
    pub(crate) fn new(
        diagnosis_id: DiagnosisId,
        action: GuardianActionKind,
        memory: Option<GuardianInstallMemoryEvidence>,
    ) -> Result<Self, GuardianInstallTerminalEvidenceError> {
        let terminal = Self {
            diagnosis_id,
            action,
            memory,
        };
        terminal.validate()?;
        Ok(terminal)
    }

    pub const fn diagnosis_id(&self) -> DiagnosisId {
        self.diagnosis_id
    }

    pub const fn action(&self) -> GuardianActionKind {
        self.action
    }

    pub fn memory(&self) -> Option<&GuardianInstallMemoryEvidence> {
        self.memory.as_ref()
    }

    pub(crate) fn validate(&self) -> Result<(), GuardianInstallTerminalEvidenceError> {
        match (self.action, &self.memory) {
            (GuardianActionKind::Retry, Some(memory)) => memory.validate(),
            (GuardianActionKind::Retry, None) | (_, Some(_)) => {
                Err(GuardianInstallTerminalEvidenceError::Shape)
            }
            (_, None) => Ok(()),
        }
    }
}

impl<'de> Deserialize<'de> for GuardianInstallTerminalEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            diagnosis_id: DiagnosisId,
            action: GuardianActionKind,
            memory: Option<GuardianInstallMemoryEvidence>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.diagnosis_id, wire.action, wire.memory)
            .map_err(|_| serde::de::Error::custom("invalid Guardian install terminal evidence"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianInstallMemoryEvidence {
    binding: GuardianMemoryBindingDigest,
    target: TargetDescriptor,
    observed_at: String,
    suppression_until: String,
}

impl<'de> Deserialize<'de> for GuardianInstallMemoryEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            binding: GuardianMemoryBindingDigest,
            target: TargetDescriptor,
            observed_at: String,
            suppression_until: String,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(
            wire.binding,
            wire.target,
            wire.observed_at,
            wire.suppression_until,
        )
        .map_err(|_| serde::de::Error::custom("invalid Guardian install memory evidence"))
    }
}

impl GuardianInstallMemoryEvidence {
    pub(crate) fn new(
        binding: GuardianMemoryBindingDigest,
        target: TargetDescriptor,
        observed_at: String,
        suppression_until: String,
    ) -> Result<Self, GuardianInstallTerminalEvidenceError> {
        let memory = Self {
            binding,
            target,
            observed_at,
            suppression_until,
        };
        memory.validate()?;
        Ok(memory)
    }

    pub const fn binding(&self) -> GuardianMemoryBindingDigest {
        self.binding
    }

    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    pub fn observed_at(&self) -> &str {
        &self.observed_at
    }

    pub fn suppression_until(&self) -> &str {
        &self.suppression_until
    }

    fn validate(&self) -> Result<(), GuardianInstallTerminalEvidenceError> {
        if self.target.system != StabilizationSystem::Execution
            || self.target.kind != TargetKind::Artifact
            || !matches!(
                self.target.ownership,
                OwnershipClass::LauncherManaged | OwnershipClass::ExternalProviderDerived
            )
            || TargetDescriptor::new(
                self.target.system,
                self.target.kind,
                &self.target.id,
                self.target.ownership,
            ) != self.target
        {
            return Err(GuardianInstallTerminalEvidenceError::Target);
        }
        let observed_at = canonical_guardian_install_timestamp(&self.observed_at)
            .ok_or(GuardianInstallTerminalEvidenceError::Window)?;
        let suppression_until = canonical_guardian_install_timestamp(&self.suppression_until)
            .ok_or(GuardianInstallTerminalEvidenceError::Window)?;
        if observed_at.checked_add_signed(chrono::Duration::minutes(5)) != Some(suppression_until) {
            return Err(GuardianInstallTerminalEvidenceError::Window);
        }
        Ok(())
    }
}

fn canonical_guardian_install_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if value.len() > 40 {
        return None;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    (parsed.to_rfc3339_opts(chrono::SecondsFormat::Millis, true) == value).then_some(parsed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct GuardianMemoryBindingDigest([u8; 32]);

impl GuardianMemoryBindingDigest {
    pub(crate) const fn from_sha256(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for GuardianMemoryBindingDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(LOWER_HEX[usize::from(byte >> 4)] as char);
            encoded.push(LOWER_HEX[usize::from(byte & 0x0f)] as char);
        }
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for GuardianMemoryBindingDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 64
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(serde::de::Error::custom(
                "Guardian memory binding must be a lowercase SHA-256 digest",
            ));
        }
        let mut digest = [0_u8; 32];
        for (output, pair) in digest.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            *output = (lower_hex_nibble(pair[0]) << 4) | lower_hex_nibble(pair[1]);
        }
        Ok(Self(digest))
    }
}

const fn lower_hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationJournalEntry {
    pub journal_id: JournalId,
    pub operation_id: OperationId,
    pub(crate) sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_operation_id: Option<OperationId>,
    pub command: CommandKind,
    pub intent: OperationIntent,
    pub status: OperationStatus,
    pub owner: StabilizationSystem,
    pub ownership: OwnershipClass,
    pub targets: Vec<TargetDescriptor>,
    pub planned_steps: Vec<OperationJournalStep>,
    pub completed_steps: Vec<OperationJournalStep>,
    pub failure_point: Option<String>,
    pub rollback: RollbackState,
    pub guardian_diagnosis_ids: Vec<DiagnosisId>,
    pub outcome: Option<OperationOutcome>,
    pub(super) reconciliation_attempt: Option<ReconciliationAttempt>,
    pub(super) reconciliation_terminal: Option<ReconciliationTerminal>,
    pub(super) persisted_state_repair_attempt: Option<PersistedStateRepairAttempt>,
    pub(super) persisted_state_repair_terminal: Option<PersistedStateRepairTerminal>,
    pub(crate) guardian_install_terminal: Option<GuardianInstallTerminalEvidence>,
}

impl OperationJournalEntry {
    pub fn new(
        journal_id: JournalId,
        operation_id: OperationId,
        command: CommandKind,
        owner: StabilizationSystem,
        ownership: OwnershipClass,
        rollback: RollbackState,
    ) -> Self {
        Self {
            journal_id,
            operation_id,
            sequence: 0,
            parent_operation_id: None,
            command,
            intent: OperationIntent::Generic {},
            status: OperationStatus::Planned,
            owner,
            ownership,
            targets: Vec::new(),
            planned_steps: Vec::new(),
            completed_steps: Vec::new(),
            failure_point: None,
            rollback,
            guardian_diagnosis_ids: Vec::new(),
            outcome: None,
            reconciliation_attempt: None,
            reconciliation_terminal: None,
            persisted_state_repair_attempt: None,
            persisted_state_repair_terminal: None,
            guardian_install_terminal: None,
        }
    }

    pub(crate) fn reconciliation_terminal(&self) -> Option<&ReconciliationTerminal> {
        self.reconciliation_terminal.as_ref()
    }

    pub fn guardian_install_terminal(&self) -> Option<&GuardianInstallTerminalEvidence> {
        self.guardian_install_terminal.as_ref()
    }

    pub(crate) fn performance_lifecycle(&self) -> Option<&PerformanceOperationLifecycle> {
        match &self.intent {
            OperationIntent::Performance(lifecycle) => Some(lifecycle),
            OperationIntent::Generic {} => None,
        }
    }

    pub(crate) fn reconciliation_attempt(&self) -> Option<&ReconciliationAttempt> {
        self.reconciliation_attempt.as_ref()
    }

    pub(crate) fn persisted_state_repair_attempt(&self) -> Option<&PersistedStateRepairAttempt> {
        self.persisted_state_repair_attempt.as_ref()
    }

    pub(crate) fn persisted_state_repair_terminal(&self) -> Option<&PersistedStateRepairTerminal> {
        self.persisted_state_repair_terminal.as_ref()
    }

    pub(crate) fn matches_store_entry(&self, expected: &Self) -> bool {
        let mut expected = expected.clone();
        expected.sequence = self.sequence;
        self == &expected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationMetricsValidationError {
    InvalidTier2IntegrityMetrics,
    ContentDownloadMetricOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier2IntegrityMetrics {
    selected_entry_count: u64,
    verified_entry_count: u64,
    processed_entry_count: u64,
    hashed_entry_count: u64,
    expected_content_byte_count: u64,
    content_read_byte_count: u64,
    metadata_lookup_count: u64,
    link_lookup_count: u64,
    suppressed_fact_count: u64,
}

impl Tier2IntegrityMetrics {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        selected_entry_count: u64,
        verified_entry_count: u64,
        processed_entry_count: u64,
        hashed_entry_count: u64,
        expected_content_byte_count: u64,
        content_read_byte_count: u64,
        metadata_lookup_count: u64,
        link_lookup_count: u64,
        suppressed_fact_count: u64,
    ) -> Result<Self, OperationMetricsValidationError> {
        let metrics = Self {
            selected_entry_count,
            verified_entry_count,
            processed_entry_count,
            hashed_entry_count,
            expected_content_byte_count,
            content_read_byte_count,
            metadata_lookup_count,
            link_lookup_count,
            suppressed_fact_count,
        };
        metrics.validate()?;
        Ok(metrics)
    }

    pub const fn selected_entry_count(&self) -> u64 {
        self.selected_entry_count
    }

    pub const fn verified_entry_count(&self) -> u64 {
        self.verified_entry_count
    }

    pub const fn processed_entry_count(&self) -> u64 {
        self.processed_entry_count
    }

    pub const fn hashed_entry_count(&self) -> u64 {
        self.hashed_entry_count
    }

    pub const fn expected_content_byte_count(&self) -> u64 {
        self.expected_content_byte_count
    }

    pub const fn content_read_byte_count(&self) -> u64 {
        self.content_read_byte_count
    }

    pub const fn metadata_lookup_count(&self) -> u64 {
        self.metadata_lookup_count
    }

    pub const fn link_lookup_count(&self) -> u64 {
        self.link_lookup_count
    }

    pub const fn suppressed_fact_count(&self) -> u64 {
        self.suppressed_fact_count
    }

    pub(crate) fn validate(&self) -> Result<(), OperationMetricsValidationError> {
        let entry_limit = MAX_TIER2_ENTRIES as u64;
        if self.selected_entry_count > entry_limit
            || self.verified_entry_count > self.processed_entry_count
            || self.processed_entry_count > self.selected_entry_count
            || self.hashed_entry_count > self.processed_entry_count
            || self.metadata_lookup_count > self.processed_entry_count
            || self
                .hashed_entry_count
                .checked_add(self.metadata_lookup_count)
                .is_none_or(|observed| observed > self.processed_entry_count)
            || self.link_lookup_count > self.metadata_lookup_count
            || self.suppressed_fact_count > self.selected_entry_count
            || self.expected_content_byte_count > MAX_TIER2_AGGREGATE_BYTES
            || self.content_read_byte_count > self.expected_content_byte_count
        {
            return Err(OperationMetricsValidationError::InvalidTier2IntegrityMetrics);
        }
        Ok(())
    }
}

macro_rules! content_download_metrics {
    ($($field:ident),+ $(,)?) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct ContentDownloadMetrics {
            $($field: u64),+
        }

        impl ContentDownloadMetrics {
            #[allow(clippy::too_many_arguments)]
            pub(crate) const fn new($($field: u64),+) -> Self {
                Self { $($field),+ }
            }

            #[cfg(test)]
            pub(crate) const fn zero() -> Self {
                Self { $($field: 0),+ }
            }

            $(
                pub const fn $field(&self) -> u64 {
                    self.$field
                }
            )+
        }
    };
}

content_download_metrics! {
    checksum_mismatch,
    metadata_invalid,
    metadata_missing,
    interrupted,
    network_failure,
    permission_failure,
    promote_failed,
    provider_failure,
    size_mismatch,
    temp_discarded,
    temp_write_failed,
    written_to_temp,
    promoted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "values",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OperationStepMetrics {
    Tier2Integrity(Tier2IntegrityMetrics),
    ContentDownload(ContentDownloadMetrics),
}

impl OperationStepMetrics {
    pub(crate) fn validate(&self) -> Result<(), OperationMetricsValidationError> {
        match self {
            Self::Tier2Integrity(metrics) => metrics.validate(),
            Self::ContentDownload(_) => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationJournalStep {
    pub step_id: String,
    pub phase: OperationPhase,
    pub result: OperationStepResult,
    pub changed_target: Option<TargetDescriptor>,
    pub generated_facts: Vec<String>,
    pub rollback: RollbackState,
    guardian_fact_ids: Vec<GuardianFactId>,
    metrics: Option<OperationStepMetrics>,
}

impl OperationJournalStep {
    pub fn new(step_id: impl Into<String>, phase: OperationPhase) -> Self {
        Self {
            step_id: step_id.into(),
            phase,
            result: OperationStepResult::Planned,
            changed_target: None,
            generated_facts: Vec::new(),
            rollback: RollbackState::NotApplicable,
            guardian_fact_ids: Vec::new(),
            metrics: None,
        }
    }

    pub fn guardian_fact_ids(&self) -> &[GuardianFactId] {
        &self.guardian_fact_ids
    }

    pub fn metrics(&self) -> Option<&OperationStepMetrics> {
        self.metrics.as_ref()
    }

    pub(crate) fn set_metrics(&mut self, metrics: OperationStepMetrics) {
        self.metrics = Some(metrics);
    }

    pub(super) fn merge_guardian_fact_ids(&mut self, fact_ids: &[GuardianFactId]) {
        for fact_id in fact_ids {
            if !self.guardian_fact_ids.contains(fact_id) {
                self.guardian_fact_ids.push(*fact_id);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_guardian_fact_ids_for_test(&mut self, fact_ids: Vec<GuardianFactId>) {
        self.guardian_fact_ids = fact_ids;
    }
}

fn contains_duplicate<T: Eq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OperationStepResult {
    Planned,
    Completed,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OperationOutcome {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum RollbackState {
    NotApplicable,
    Unavailable,
    Available,
    Applied,
}

#[cfg(test)]
mod tests {
    use super::{
        CommandKind, ContentDownloadMetrics, DurableGuardianEvidence, DurableGuardianEvidenceError,
        GuardianInstallMemoryEvidence, GuardianInstallTerminalEvidence,
        GuardianInstallTerminalEvidenceError, GuardianMemoryBindingDigest, JournalId, OperationId,
        OperationJournalEntry, OperationJournalStep, OperationOutcome, OperationPhase,
        OperationStatus, OwnershipClass, PersistedStateRecordStore, ReconciliationAttempt,
        ReconciliationComponent, ReconciliationIncarnationFingerprint,
        ReconciliationInventoryFingerprint, ReconciliationLineage,
        ReconciliationQuarantineCheckpoint, ReconciliationQuarantineRecord, ReconciliationRung,
        ReconciliationScope, ReconciliationTerminal, ReconciliationTerminalOutcome,
        ReconciliationTerminalValidationError, ReconciliationVersionBundleOutcome,
        RestartStableRecordIdentity, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind, Tier2IntegrityMetrics,
    };
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianDomain, GuardianFactId, GuardianMode,
    };
    use static_assertions::assert_not_impl_any;
    use std::collections::HashSet;
    use std::path::Path;

    assert_not_impl_any!(
        RestartStableRecordIdentity:
            AsRef<Path>,
            AsRef<[u8]>
    );
    assert_not_impl_any!(OperationId: Copy, Default, AsRef<str>, From<String>);

    #[test]
    fn behavior_contract_failure_memory_ids_preserve_exact_historical_literals() {
        for system in StabilizationSystem::ALL {
            assert_eq!(system.failure_memory_id(), format!("{system:?}"));
        }
        for kind in TargetKind::ALL {
            assert_eq!(kind.failure_memory_id(), format!("{kind:?}"));
        }
        for ownership in OwnershipClass::ALL {
            assert_eq!(ownership.failure_memory_id(), format!("{ownership:?}"));
        }
        for rung in ReconciliationRung::ALL {
            assert_eq!(rung.failure_memory_id(), format!("{rung:?}"));
        }
        for component in ReconciliationComponent::ALL {
            assert_eq!(component.failure_memory_id(), format!("{component:?}"));
        }
        for store in PersistedStateRecordStore::ALL {
            assert_eq!(store.failure_memory_id(), format!("{store:?}"));
        }
    }

    #[test]
    fn behavior_contract_durable_evidence_is_bounded_unique_and_operation_owned() {
        let operation_id = OperationId::deterministic_test("evidence-contracts-durable-evidence");
        let evidence = DurableGuardianEvidence::new(
            operation_id.clone(),
            vec![GuardianFactId::ArtifactMissing],
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            None,
        )
        .expect("valid evidence projection");
        assert_eq!(evidence.operation_id(), &operation_id);
        assert_eq!(evidence.fact_ids(), [GuardianFactId::ArtifactMissing]);
        assert_eq!(
            evidence.diagnosis_ids(),
            [DiagnosisId::LauncherManagedArtifactCorrupt]
        );
        assert_eq!(
            DurableGuardianEvidence::new(operation_id.clone(), Vec::new(), Vec::new(), None),
            Err(DurableGuardianEvidenceError::Empty)
        );
        assert_eq!(
            DurableGuardianEvidence::new(
                operation_id,
                vec![GuardianFactId::ArtifactMissing; 2],
                Vec::new(),
                None,
            ),
            Err(DurableGuardianEvidenceError::DuplicateFact)
        );

        let operation_id =
            OperationId::deterministic_test("evidence-contracts-exact-evidence-caps");
        let exact_facts = GuardianFactId::ALL[..super::MAX_DURABLE_GUARDIAN_FACT_IDS].to_vec();
        let exact_diagnoses = DiagnosisId::ALL[..super::MAX_DURABLE_GUARDIAN_DIAGNOSES].to_vec();
        assert!(
            DurableGuardianEvidence::new(operation_id.clone(), exact_facts, exact_diagnoses, None,)
                .is_ok(),
            "the exact closed evidence limits must remain admissible"
        );
        assert_eq!(
            DurableGuardianEvidence::new(
                operation_id.clone(),
                GuardianFactId::ALL[..=super::MAX_DURABLE_GUARDIAN_FACT_IDS].to_vec(),
                Vec::new(),
                None,
            ),
            Err(DurableGuardianEvidenceError::TooManyFacts)
        );
        assert_eq!(
            DurableGuardianEvidence::new(
                operation_id,
                vec![GuardianFactId::ArtifactMissing],
                DiagnosisId::ALL[..=super::MAX_DURABLE_GUARDIAN_DIAGNOSES].to_vec(),
                None,
            ),
            Err(DurableGuardianEvidenceError::TooManyDiagnoses)
        );
    }

    #[test]
    fn behavior_contract_install_memory_requires_millis_z_and_exact_five_minutes() {
        let target = TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "minecraft_client_1.21.5",
            OwnershipClass::LauncherManaged,
        );
        let digest = GuardianMemoryBindingDigest::from_sha256([0x5a; 32]);
        let memory = GuardianInstallMemoryEvidence::new(
            digest,
            target.clone(),
            "2026-08-13T10:00:00.000Z".to_string(),
            "2026-08-13T10:05:00.000Z".to_string(),
        )
        .expect("canonical exact five-minute memory");
        assert_eq!(memory.binding().as_bytes(), &[0x5a; 32]);
        assert_eq!(memory.target(), &target);
        assert_eq!(
            GuardianInstallMemoryEvidence::new(
                digest,
                target.clone(),
                "2026-08-13T12:00:00.000+02:00".to_string(),
                "2026-08-13T12:05:00.000+02:00".to_string(),
            ),
            Err(GuardianInstallTerminalEvidenceError::Window)
        );
        assert_eq!(
            GuardianInstallMemoryEvidence::new(
                digest,
                target,
                "2026-08-13T10:00:00Z".to_string(),
                "2026-08-13T10:05:00Z".to_string(),
            ),
            Err(GuardianInstallTerminalEvidenceError::Window)
        );
        let terminal = GuardianInstallTerminalEvidence::new(
            DiagnosisId::DownloadUnavailable,
            GuardianActionKind::Retry,
            Some(memory),
        )
        .expect("Retry owns exact memory");
        assert_eq!(terminal.action(), GuardianActionKind::Retry);
    }

    #[test]
    fn behavior_contract_install_terminal_deserialization_preserves_private_invariants() {
        let memory = serde_json::json!({
            "binding": "5a".repeat(32),
            "target": {
                "system": "Execution",
                "kind": "Artifact",
                "id": "minecraft_client_1.21.5",
                "ownership": "LauncherManaged"
            },
            "observed_at": "2026-08-13T10:00:00.000Z",
            "suppression_until": "2026-08-13T10:05:00.000Z"
        });
        let valid = serde_json::json!({
            "diagnosis_id": "download_unavailable",
            "action": "Retry",
            "memory": memory
        });
        assert!(serde_json::from_value::<GuardianInstallTerminalEvidence>(valid.clone()).is_ok());

        let mut missing_memory = valid.clone();
        missing_memory["memory"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<GuardianInstallTerminalEvidence>(missing_memory).is_err());

        let mut memory_on_block = valid.clone();
        memory_on_block["action"] = serde_json::Value::String("Block".to_string());
        assert!(
            serde_json::from_value::<GuardianInstallTerminalEvidence>(memory_on_block).is_err()
        );

        let mut noncanonical_timestamp = valid.clone();
        noncanonical_timestamp["memory"]["observed_at"] =
            serde_json::Value::String("2026-08-13T10:00:00Z".to_string());
        assert!(
            serde_json::from_value::<GuardianInstallTerminalEvidence>(noncanonical_timestamp)
                .is_err()
        );

        let mut uppercase_binding = valid.clone();
        uppercase_binding["memory"]["binding"] = serde_json::Value::String("5A".repeat(32));
        assert!(
            serde_json::from_value::<GuardianInstallTerminalEvidence>(uppercase_binding).is_err()
        );

        let mut unknown = valid;
        unknown["future"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<GuardianInstallTerminalEvidence>(unknown).is_err());
    }

    #[test]
    fn behavior_contract_tier2_metrics_reject_invalid_shapes() {
        assert!(Tier2IntegrityMetrics::new(1, 1, 1, 1, 1, 1, 0, 0, 0).is_ok());
        assert!(Tier2IntegrityMetrics::new(1, 2, 1, 1, 1, 1, 0, 0, 0).is_err());
        assert!(
            Tier2IntegrityMetrics::new(2, 1, 1, 1, 1, 1, 1, 1, 0).is_err(),
            "hash plus metadata work cannot exceed processed entries"
        );
    }

    #[test]
    fn behavior_contract_content_metrics_round_trip_exact_u64_maximum() {
        let maximum = ContentDownloadMetrics::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
        );
        let encoded = serde_json::to_string(&maximum).expect("encode exact u64 maxima");
        let decoded = serde_json::from_str::<ContentDownloadMetrics>(&encoded)
            .expect("decode exact u64 maxima");
        assert_eq!(decoded, maximum);
    }

    #[test]
    fn operation_id_has_one_exact_wire_representation() {
        let canonical = "op-550e8400-e29b-41d4-a716-446655440000";
        let operation_id = canonical.parse::<OperationId>().expect("canonical UUID v4");
        assert_eq!(operation_id.to_string(), canonical);
        assert_eq!(
            serde_json::to_string(&operation_id).expect("serialize operation id"),
            format!("\"{canonical}\"")
        );
        assert_eq!(
            serde_json::from_str::<OperationId>(&format!("\"{canonical}\""))
                .expect("deserialize operation id"),
            operation_id
        );

        for invalid in [
            "550e8400-e29b-41d4-a716-446655440000",
            "op-550E8400-E29B-41D4-A716-446655440000",
            "op-{550e8400-e29b-41d4-a716-446655440000}",
            "op-550e8400e29b41d4a716446655440000",
            "op-550e8400-e29b-31d4-a716-446655440000",
            "op-550e8400-e29b-41d4-7716-446655440000",
            " op-550e8400-e29b-41d4-a716-446655440000",
            "op-550e8400-e29b-41d4-a716-446655440000 ",
            "op-550e8400-e29b-41d4-a716-446655440000\n",
            "op-550e8400-e29b-41d4-a716-44665544000/",
            "op-550e8400-e29b-41d4-a716-44665544000é",
        ] {
            assert!(
                invalid.parse::<OperationId>().is_err(),
                "accepted {invalid:?}"
            );
            assert!(
                serde_json::from_value::<OperationId>(serde_json::json!(invalid)).is_err(),
                "serde accepted {invalid:?}"
            );
        }
        assert!(serde_json::from_str::<OperationId>("7").is_err());
        assert!(serde_json::from_str::<OperationId>("null").is_err());
    }

    #[test]
    fn minted_operation_ids_are_canonical_and_unique() {
        let mut observed = HashSet::with_capacity(10_000);
        for _ in 0..10_000 {
            let operation_id = OperationId::mint();
            let encoded = operation_id.to_string();
            assert_eq!(encoded.len(), 39);
            assert_eq!(encoded.parse::<OperationId>(), Ok(operation_id.clone()));
            assert!(
                observed.insert(operation_id),
                "minted a duplicate operation id"
            );
        }
    }

    #[test]
    fn operation_identity_sources_have_no_fallback_or_derived_generators() {
        let sources = [
            include_str!("../application/install.rs"),
            include_str!("../application/install/operation.rs"),
            include_str!("../application/performance.rs"),
            include_str!("../guardian/healing.rs"),
            include_str!("../guardian/preflight.rs"),
            include_str!("../observability/mod.rs"),
        ];
        for source in sources {
            for forbidden in [
                "OperationId::new",
                "pub fn install_operation_id(",
                "fn safe_operation_id",
                "fn public_safe_operation_id",
                "generate_performance_operation_id",
                "-reconciliation",
            ] {
                assert!(!source.contains(forbidden), "found forbidden {forbidden}");
            }
        }
    }

    #[test]
    fn restart_record_identity_and_benchmark_store_have_strict_durable_shapes() {
        let identity = RestartStableRecordIdentity::from_digest([0xab; 32]);
        let encoded = serde_json::to_string(&identity).expect("serialize restart identity");
        assert_eq!(
            encoded,
            "\"sha256.abababab.abababab.abababab.abababab.abababab.abababab.abababab.abababab\""
        );
        assert_eq!(
            serde_json::from_str::<RestartStableRecordIdentity>(&encoded)
                .expect("deserialize canonical restart identity"),
            identity
        );
        for invalid in [
            "sha256.abababababababababababababababababababababababababababababababab",
            "sha256.ABABABAB.abababab.abababab.abababab.abababab.abababab.abababab.abababab",
            "sha256.abababab.abababab.abababab.abababab.abababab.abababab.abababab",
        ] {
            assert!(
                serde_json::from_value::<RestartStableRecordIdentity>(serde_json::json!(invalid))
                    .is_err()
            );
        }

        assert_eq!(
            serde_json::to_string(&PersistedStateRecordStore::BenchmarkSuiteDriver)
                .expect("serialize driver store"),
            "\"benchmark_suite_driver\""
        );
        assert!(
            serde_json::from_str::<PersistedStateRecordStore>("\"PerformanceOperation\"").is_err()
        );
    }

    fn reconciliation_attempt(
        rung: ReconciliationRung,
        component: ReconciliationComponent,
        lineage: ReconciliationLineage,
    ) -> ReconciliationAttempt {
        reconciliation_attempt_with_policy(
            rung,
            component,
            lineage,
            OwnershipClass::LauncherManaged,
            OwnershipClass::LauncherManaged,
            GuardianMode::Managed,
        )
    }

    fn reconciliation_attempt_with_policy(
        rung: ReconciliationRung,
        component: ReconciliationComponent,
        lineage: ReconciliationLineage,
        ownership: OwnershipClass,
        target_ownership: OwnershipClass,
        mode: GuardianMode,
    ) -> ReconciliationAttempt {
        let (system, kind, id) = if component == ReconciliationComponent::Runtime {
            (
                StabilizationSystem::Execution,
                TargetKind::Runtime,
                "java-runtime-delta",
            )
        } else {
            (
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "managed-artifact",
            )
        };
        ReconciliationAttempt::new(
            OperationId::deterministic_test(format!("attempt-{rung:?}")),
            DiagnosisId::LauncherManagedArtifactCorrupt,
            GuardianDomain::Library,
            rung,
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
            component,
            TargetDescriptor::new(system, kind, id, target_ownership),
            mode,
            ownership,
            "2026-07-15T00:00:00Z",
            "2026-07-15T01:00:00Z",
            lineage,
        )
    }

    fn install_publication_evidence() -> axial_minecraft::ManagedInstallPublicationEvidenceId {
        axial_minecraft::ManagedInstallPublicationEvidenceId::parse(
            "managed-install-v1.T7ghN0PBffcxr4Rg08bVvTPOl9fRcUh9qyNnWZtd93c.Xsu8KmJnT7So_J1WS8rcqA.X-fR4EpDTc2mbfPpfNOFiA.JGoynsQN9LfT8e7hWyX1fknDskeaM7xQCAbFGATbD-I._FMcn_pUsOarv_sNtTJousevn4S1SMqttV6yiYdONOY",
        )
        .expect("canonical install publication evidence")
    }

    #[test]
    fn operation_journal_entry_round_trips_strict_shape() {
        let mut entry = OperationJournalEntry::new(
            JournalId::new("journal-1"),
            OperationId::deterministic_test("operation-1"),
            CommandKind::RefreshPerformanceRules,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.status = OperationStatus::Succeeded;
        entry.targets.push(TargetDescriptor::new(
            StabilizationSystem::Performance,
            TargetKind::Config,
            "performance_rules_cache",
            OwnershipClass::LauncherManaged,
        ));
        entry.planned_steps.push(OperationJournalStep::new(
            "refresh_remote_rules",
            OperationPhase::Running,
        ));
        let mut completed =
            OperationJournalStep::new("refresh_remote_rules", OperationPhase::Running);
        completed.result = super::OperationStepResult::Completed;
        entry.completed_steps.push(completed);
        entry.outcome = Some(OperationOutcome::Succeeded);

        let encoded = serde_json::to_string(&entry).expect("serialize journal");
        let decoded: OperationJournalEntry =
            serde_json::from_str(&encoded).expect("deserialize journal");

        assert_eq!(decoded, entry);
    }

    #[test]
    fn operation_journal_entry_rejects_unknown_fields() {
        let operation_id = OperationId::try_from("op-123e4567-e89b-42d3-a456-426614174000")
            .expect("canonical operation id");
        let mut entry = OperationJournalEntry::new(
            JournalId::new(format!("journal-{operation_id}")),
            operation_id,
            CommandKind::RefreshPerformanceRules,
            StabilizationSystem::Application,
            OwnershipClass::LauncherManaged,
            RollbackState::NotApplicable,
        );
        entry.sequence = 1;
        let mut value = serde_json::to_value(entry).expect("journal value");
        value
            .as_object_mut()
            .expect("journal entry object")
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));

        let result = serde_json::from_value::<OperationJournalEntry>(value);

        assert!(result.is_err());
    }

    #[test]
    fn target_descriptor_constructor_sanitizes_sensitive_ids() {
        let descriptor = TargetDescriptor::new(
            StabilizationSystem::State,
            TargetKind::FilesystemPath,
            r"C:\Users\Alice\AppData\Local\java.exe",
            OwnershipClass::UserOwned,
        );
        let encoded = serde_json::to_string(&descriptor).expect("serialize target descriptor");

        assert_eq!(descriptor.id, "target");
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("java.exe"));
        assert!(!encoded.contains(r"C:\"));
    }

    #[test]
    fn successor_lineage_is_mandatory_and_adjacent_by_shape() {
        let repair = reconciliation_attempt(
            ReconciliationRung::RepairArtifact,
            ReconciliationComponent::Libraries,
            ReconciliationLineage::Initial,
        );
        assert!(repair.validate().is_ok());

        let invalid = reconciliation_attempt(
            ReconciliationRung::RebuildComponent,
            ReconciliationComponent::Libraries,
            ReconciliationLineage::Initial,
        );
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn behavior_contract_every_reconciliation_rung_rejects_unowned_and_non_managed_attempts() {
        let rung_shapes = [
            (
                ReconciliationRung::RepairArtifact,
                ReconciliationComponent::Libraries,
                ReconciliationLineage::Initial,
            ),
            (
                ReconciliationRung::RebuildComponent,
                ReconciliationComponent::Libraries,
                ReconciliationLineage::Predecessor {
                    operation_id: OperationId::deterministic_test("repair-attempt"),
                },
            ),
        ];
        let unowned = [
            OwnershipClass::CompositionManaged,
            OwnershipClass::UserOwned,
            OwnershipClass::ExternalProviderDerived,
            OwnershipClass::Unknown,
        ];

        for (rung, component, lineage) in rung_shapes {
            assert_eq!(
                reconciliation_attempt_with_policy(
                    rung,
                    component,
                    lineage.clone(),
                    OwnershipClass::LauncherManaged,
                    OwnershipClass::LauncherManaged,
                    GuardianMode::Managed,
                )
                .validate(),
                Ok(()),
                "{rung:?} baseline must remain a valid durable shape"
            );
            for ownership in unowned {
                let attempt = reconciliation_attempt_with_policy(
                    rung,
                    component,
                    lineage.clone(),
                    ownership,
                    OwnershipClass::LauncherManaged,
                    GuardianMode::Managed,
                );
                assert_eq!(
                    attempt.validate(),
                    Err(ReconciliationTerminalValidationError::UnsafeOwnership),
                    "{rung:?} must reject {ownership:?} attempt ownership"
                );

                let attempt = reconciliation_attempt_with_policy(
                    rung,
                    component,
                    lineage.clone(),
                    OwnershipClass::LauncherManaged,
                    ownership,
                    GuardianMode::Managed,
                );
                assert_eq!(
                    attempt.validate(),
                    Err(ReconciliationTerminalValidationError::UnsafeTarget),
                    "{rung:?} must reject {ownership:?} target ownership"
                );
            }

            for mode in [GuardianMode::Custom, GuardianMode::Disabled] {
                let non_managed = reconciliation_attempt_with_policy(
                    rung,
                    component,
                    lineage.clone(),
                    OwnershipClass::LauncherManaged,
                    OwnershipClass::LauncherManaged,
                    mode,
                );
                assert_eq!(
                    non_managed.validate(),
                    Err(ReconciliationTerminalValidationError::NonManagedMode),
                    "{rung:?} must reject {mode:?} before persistence"
                );
            }
        }
    }

    #[test]
    fn quarantine_checkpoint_is_typed_bounded_and_duplicate_free() {
        let runtime_attempt = reconciliation_attempt(
            ReconciliationRung::RebuildComponent,
            ReconciliationComponent::Runtime,
            ReconciliationLineage::Predecessor {
                operation_id: OperationId::deterministic_test("component-attempt"),
            },
        );
        let runtime = ReconciliationQuarantineRecord::runtime("java-runtime-delta");
        let valid = ReconciliationTerminal::from_attempt(
            runtime_attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::new(vec![runtime.clone()]),
        );
        assert!(valid.validate().is_ok());

        let duplicate = ReconciliationTerminal::from_attempt(
            runtime_attempt.clone(),
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::new(vec![runtime.clone(), runtime.clone()]),
        );
        assert!(duplicate.validate().is_err());

        let overflow = ReconciliationTerminal::from_attempt(
            runtime_attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::new(
                (0..=super::RECONCILIATION_QUARANTINE_CAPACITY)
                    .map(|index| {
                        ReconciliationQuarantineRecord::runtime(format!(
                            "java-runtime-test-{index}"
                        ))
                    })
                    .collect(),
            ),
        );
        assert!(overflow.validate().is_err());
    }

    #[test]
    fn version_bundle_terminal_requires_exact_publication_checkpoint_after_effect() {
        let attempt = reconciliation_attempt(
            ReconciliationRung::RebuildComponent,
            ReconciliationComponent::VersionBundle,
            ReconciliationLineage::Predecessor {
                operation_id: OperationId::deterministic_test("version-bundle-predecessor"),
            },
        );
        let missing = ReconciliationTerminal::from_attempt(
            attempt.clone(),
            ReconciliationTerminalOutcome::Succeeded,
            ReconciliationQuarantineCheckpoint::default(),
        );
        assert_eq!(
            missing.validate(),
            Err(ReconciliationTerminalValidationError::InvalidPublication)
        );

        let committed = missing.clone().with_version_bundle_publication(
            install_publication_evidence(),
            ReconciliationVersionBundleOutcome::Committed,
        );
        assert!(committed.validate().is_ok());
        assert!(
            committed
                .version_bundle_publication()
                .expect("pending publication")
                .is_pending()
        );
        let acknowledged = committed
            .with_acknowledged_version_bundle_publication(&install_publication_evidence())
            .expect("acknowledge exact publication");
        assert!(
            !acknowledged
                .version_bundle_publication()
                .expect("acknowledged publication")
                .is_pending()
        );

        let impossible_success = missing.with_version_bundle_publication(
            install_publication_evidence(),
            ReconciliationVersionBundleOutcome::RolledBack,
        );
        assert_eq!(
            impossible_success.validate(),
            Err(ReconciliationTerminalValidationError::InvalidPublication)
        );

        let pre_effect_failure = ReconciliationTerminal::from_attempt(
            attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
        );
        assert!(pre_effect_failure.validate().is_ok());

        let artifact_attempt = reconciliation_attempt(
            ReconciliationRung::RepairArtifact,
            ReconciliationComponent::VersionBundle,
            ReconciliationLineage::Initial,
        );
        let impossible = ReconciliationTerminal::from_attempt(
            artifact_attempt,
            ReconciliationTerminalOutcome::Failed,
            ReconciliationQuarantineCheckpoint::default(),
        )
        .with_version_bundle_publication(
            install_publication_evidence(),
            ReconciliationVersionBundleOutcome::RolledBack,
        );
        assert_eq!(
            impossible.validate(),
            Err(ReconciliationTerminalValidationError::InvalidPublication)
        );
    }
}
