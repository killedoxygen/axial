//! State system contracts.
//!
//! The existing `state` module still owns runtime `AppState` and current stores.
//! This submodule exposes the durable vocabulary for journals, ownership,
//! snapshots, and persistence boundaries used by the target systems.

use crate::guardian::{DiagnosisId, GuardianDomain, GuardianMode};
use crate::observability::evidence_text_looks_sensitive;
use serde::{Deserialize, Serialize};

pub(crate) const RECONCILIATION_EVIDENCE_CAPACITY: usize = 128;

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

impl ReconciliationIncarnationFingerprint {
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
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ReconciliationTerminalOutcome {
    Succeeded,
    Failed,
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationTerminal {
    attempt: ReconciliationAttempt,
    outcome: ReconciliationTerminalOutcome,
    quarantined_target: Option<TargetDescriptor>,
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
        } = &self.scope;
        if !axial_config::is_canonical_instance_id(instance_id) {
            return Err(ReconciliationTerminalValidationError::UnsafeInstanceId);
        }
        if !valid_reconciliation_fingerprint(fingerprint.as_str()) {
            return Err(ReconciliationTerminalValidationError::UnsafeFingerprint);
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
        quarantined_target: Option<TargetDescriptor>,
    ) -> Self {
        Self {
            attempt,
            outcome,
            quarantined_target,
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

    pub fn quarantined_target(&self) -> Option<&TargetDescriptor> {
        self.quarantined_target.as_ref()
    }

    pub(super) fn validate(&self) -> Result<(), ReconciliationTerminalValidationError> {
        self.attempt.validate()?;
        if let Some(target) = &self.quarantined_target {
            let expected = TargetDescriptor::new(
                StabilizationSystem::Execution,
                self.target().kind,
                format!("quarantine-{}", self.target().id),
                self.ownership(),
            );
            if target != &expected {
                return Err(ReconciliationTerminalValidationError::UnsafeTarget);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReconciliationTerminalValidationError {
    UnsafeOperationId,
    UnsafeInstanceId,
    UnsafeFingerprint,
    UnsafeOwnership,
    UnsafeTarget,
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
        OperationOutcome, OperationPhase, OperationStatus, OwnershipClass, RollbackState,
        StabilizationSystem, TargetDescriptor, TargetKind,
    };

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
}
