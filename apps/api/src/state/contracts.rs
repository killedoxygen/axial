//! State system contracts.
//!
//! The existing `state` module still owns runtime `AppState` and current stores.
//! This submodule exposes the durable vocabulary for journals, ownership,
//! snapshots, and persistence boundaries used by the target systems.

use crate::guardian::{DiagnosisId, GuardianDomain, GuardianMode};
use crate::observability::evidence_text_looks_sensitive;
use serde::{Deserialize, Serialize};

pub(crate) const RECONCILIATION_EVIDENCE_CAPACITY: usize = 128;
pub const RECONCILIATION_QUARANTINE_CAPACITY: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistedStateRecordStore {
    PerformanceOperation,
    BenchmarkSuiteDriver,
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

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct OperationId(pub String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReconciliationRung {
    RepairArtifact,
    RebuildComponent,
    RematerializeInstance,
}

const RECONCILIATION_MAX_ATTEMPTS_PER_SUPPRESSION_WINDOW: usize = 1;

impl ReconciliationRung {
    pub const ALL: &'static [Self] = &[
        Self::RepairArtifact,
        Self::RebuildComponent,
        Self::RematerializeInstance,
    ];

    pub(crate) const fn max_attempts_per_suppression_window(self) -> usize {
        match self {
            Self::RepairArtifact | Self::RebuildComponent | Self::RematerializeInstance => {
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
    WholeInstance,
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
                        || !matches!(
                            attempt.component(),
                            ReconciliationComponent::Runtime
                                | ReconciliationComponent::WholeInstance
                        )
                        || (attempt.component() == ReconciliationComponent::Runtime
                            && attempt.target().id != *component_id)
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
        if !safe_reconciliation_token(self.operation_id.as_str(), 128) {
            return Err(ReconciliationTerminalValidationError::UnsafeOperationId);
        }
        if self.ownership != OwnershipClass::LauncherManaged {
            return Err(ReconciliationTerminalValidationError::UnsafeOwnership);
        }
        if self.target.ownership != self.ownership
            || self.target.id.trim().is_empty()
            || self.target.id.contains(['/', '\\'])
        {
            return Err(ReconciliationTerminalValidationError::UnsafeTarget);
        }
        if self.mode == GuardianMode::Disabled {
            return Err(ReconciliationTerminalValidationError::DisabledMode);
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
                ReconciliationRung::RebuildComponent | ReconciliationRung::RematerializeInstance,
            ) if operation_id != &self.operation_id
                && safe_reconciliation_token(operation_id.as_str(), 128) => {}
            _ => return Err(ReconciliationTerminalValidationError::InvalidLineage),
        }
        match (self.rung, self.component) {
            (
                ReconciliationRung::RepairArtifact | ReconciliationRung::RebuildComponent,
                ReconciliationComponent::VersionBundle
                | ReconciliationComponent::Libraries
                | ReconciliationComponent::Assets
                | ReconciliationComponent::Runtime,
            )
            | (ReconciliationRung::RematerializeInstance, ReconciliationComponent::WholeInstance) => {
                Ok(())
            }
            _ => Err(ReconciliationTerminalValidationError::ImpossibleComponent),
        }?;
        match self.component {
            ReconciliationComponent::VersionBundle
                if matches!(self.target.kind, TargetKind::Artifact | TargetKind::Version) => {}
            ReconciliationComponent::Libraries | ReconciliationComponent::Assets
                if self.target.kind == TargetKind::Artifact => {}
            ReconciliationComponent::Runtime if self.target.kind == TargetKind::Runtime => {}
            ReconciliationComponent::WholeInstance => {
                let ReconciliationScope::RegisteredInstance { instance_id, .. } = &self.scope;
                if self.target.system != StabilizationSystem::State
                    || self.target.kind != TargetKind::Instance
                    || self.target.id != *instance_id
                {
                    return Err(ReconciliationTerminalValidationError::ImpossibleComponent);
                }
            }
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
        }
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

    pub(super) fn validate(&self) -> Result<(), ReconciliationTerminalValidationError> {
        self.attempt.validate()?;
        self.quarantine_checkpoint.validate_for(&self.attempt)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReconciliationTerminalValidationError {
    UnsafeOperationId,
    UnsafeInstanceId,
    UnsafeFingerprint,
    UnsafeInventoryFingerprint,
    UnsafeOwnership,
    UnsafeTarget,
    TooManyQuarantines,
    InvalidLineage,
    DisabledMode,
    InvalidWindow,
    ImpossibleComponent,
}

fn safe_reconciliation_token(value: &str, max_chars: usize) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= max_chars
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '+')
        })
}

fn valid_reconciliation_fingerprint(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256.") else {
        return false;
    };
    let segments = digest.split('.').collect::<Vec<_>>();
    segments.len() == 8
        && segments.iter().all(|segment| {
            segment.len() == 8 && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum CommandKind {
    CreateInstance,
    LaunchInstance,
    StopSession,
    InstallVersion,
    RepairInstance,
    ApplyPerformancePlan,
    RefreshPerformanceRules,
    ValidateInstance,
    OpenInstanceFolder,
    RefreshAccountReadiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OwnershipClass {
    LauncherManaged,
    CompositionManaged,
    UserOwned,
    ExternalProviderDerived,
    Unknown,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationJournalEntry {
    pub journal_id: JournalId,
    pub operation_id: OperationId,
    pub command: CommandKind,
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
            command,
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
        }
    }

    pub(crate) fn reconciliation_terminal(&self) -> Option<&ReconciliationTerminal> {
        self.reconciliation_terminal.as_ref()
    }

    pub(crate) fn reconciliation_attempt(&self) -> Option<&ReconciliationAttempt> {
        self.reconciliation_attempt.as_ref()
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
        }
    }
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotDescriptor {
    pub target: TargetDescriptor,
    pub ownership: OwnershipClass,
    pub rollback: RollbackState,
}

#[cfg(test)]
mod tests {
    use super::{
        CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass,
        PersistedStateRecordStore, ReconciliationAttempt, ReconciliationComponent,
        ReconciliationIncarnationFingerprint, ReconciliationInventoryFingerprint,
        ReconciliationLineage, ReconciliationQuarantineCheckpoint, ReconciliationQuarantineRecord,
        ReconciliationRung, ReconciliationScope, ReconciliationTerminal,
        ReconciliationTerminalOutcome, ReconciliationTerminalValidationError,
        RestartStableRecordIdentity, RollbackState, StabilizationSystem, TargetDescriptor,
        TargetKind,
    };
    use crate::guardian::{DiagnosisId, GuardianDomain, GuardianMode};
    use static_assertions::assert_not_impl_any;
    use std::path::Path;

    assert_not_impl_any!(
        RestartStableRecordIdentity:
            AsRef<Path>,
            AsRef<[u8]>
    );

    #[test]
    fn restart_record_identity_and_store_have_strict_durable_shapes() {
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
            serde_json::to_string(&PersistedStateRecordStore::PerformanceOperation)
                .expect("serialize performance store"),
            "\"performance_operation\""
        );
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
        let (system, kind, id) = if component == ReconciliationComponent::WholeInstance {
            (
                StabilizationSystem::State,
                TargetKind::Instance,
                "0123456789abcdef",
            )
        } else if component == ReconciliationComponent::Runtime {
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
            OperationId::new(format!("attempt-{rung:?}")),
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

    #[test]
    fn operation_journal_entry_round_trips_strict_shape() {
        let mut entry = OperationJournalEntry::new(
            JournalId::new("journal-1"),
            OperationId::new("operation-1"),
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
        let value = serde_json::json!({
            "journal_id": "journal-1",
            "operation_id": "operation-1",
            "command": "RefreshPerformanceRules",
            "status": "Succeeded",
            "owner": "Application",
            "ownership": "LauncherManaged",
            "targets": [],
            "planned_steps": [],
            "completed_steps": [],
            "failure_point": null,
            "rollback": "NotApplicable",
            "guardian_diagnosis_ids": [],
            "outcome": "Succeeded",
            "unexpected": true
        });

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

        let whole = reconciliation_attempt(
            ReconciliationRung::RematerializeInstance,
            ReconciliationComponent::WholeInstance,
            ReconciliationLineage::Predecessor {
                operation_id: OperationId::new("component-attempt"),
            },
        );
        assert!(whole.validate().is_ok());
    }

    #[test]
    fn every_reconciliation_rung_rejects_unowned_and_disabled_attempts() {
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
                    operation_id: OperationId::new("repair-attempt"),
                },
            ),
            (
                ReconciliationRung::RematerializeInstance,
                ReconciliationComponent::WholeInstance,
                ReconciliationLineage::Predecessor {
                    operation_id: OperationId::new("component-attempt"),
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

            let disabled = reconciliation_attempt_with_policy(
                rung,
                component,
                lineage,
                OwnershipClass::LauncherManaged,
                OwnershipClass::LauncherManaged,
                GuardianMode::Disabled,
            );
            assert_eq!(
                disabled.validate(),
                Err(ReconciliationTerminalValidationError::DisabledMode),
                "{rung:?} must reject Disabled mode before persistence"
            );
        }
    }

    #[test]
    fn quarantine_checkpoint_is_typed_bounded_and_duplicate_free() {
        let runtime_attempt = reconciliation_attempt(
            ReconciliationRung::RematerializeInstance,
            ReconciliationComponent::WholeInstance,
            ReconciliationLineage::Predecessor {
                operation_id: OperationId::new("component-attempt"),
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
}
