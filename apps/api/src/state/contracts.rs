//! State system contracts.
//!
//! The existing `state` module still owns runtime `AppState` and current stores.
//! This submodule exposes the durable vocabulary for journals, ownership,
//! snapshots, and persistence boundaries used by the target systems.

use serde::{Deserialize, Serialize};

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
    if value.is_empty() || target_id_looks_sensitive(value) {
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

fn target_id_looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || lower.contains("-xmx")
        || lower.contains("-xms")
        || lower.contains("-xx:")
        || lower.contains("--access")
        || lower.contains("--username")
        || lower.contains("--uuid")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || has_windows_drive_prefix(value)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum OperationPhase {
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
    pub guardian_diagnosis_ids: Vec<String>,
    pub outcome: Option<OperationOutcome>,
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
