//! Application command catalog and request/result contracts.
//!
//! These types name backend-owned use-case inputs and outputs before route
//! cutover. They do not execute workflows; they keep command meaning out of
//! frontend code and out of lower-level effect systems.

use super::{ApplicationCommand, CommandResult, CommandResultCarriers, EmptyPayload};
use crate::state::contracts::{
    CommandKind, OperationId, OperationStatus, OwnershipClass, StabilizationSystem,
    TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};

const PHASE_ONE_COMMAND_KINDS: &[CommandKind] = &[
    CommandKind::LaunchInstance,
    CommandKind::InstallVersion,
    CommandKind::RepairInstance,
    CommandKind::ApplyPerformancePlan,
    CommandKind::RefreshPerformanceRules,
    CommandKind::StopSession,
    CommandKind::ValidateInstance,
    CommandKind::RefreshAccountReadiness,
];

const GUARDIAN_OPERATION_SESSION_VIEW: &[CommandResultCarrierKind] = &[
    CommandResultCarrierKind::Guardian,
    CommandResultCarrierKind::Operation,
    CommandResultCarrierKind::Session,
    CommandResultCarrierKind::ViewModel,
];
const GUARDIAN_OPERATION_VIEW: &[CommandResultCarrierKind] = &[
    CommandResultCarrierKind::Guardian,
    CommandResultCarrierKind::Operation,
    CommandResultCarrierKind::ViewModel,
];
const GUARDIAN_PERFORMANCE_OPERATION_VIEW: &[CommandResultCarrierKind] = &[
    CommandResultCarrierKind::Guardian,
    CommandResultCarrierKind::Performance,
    CommandResultCarrierKind::Operation,
    CommandResultCarrierKind::ViewModel,
];
const PERFORMANCE_OPERATION_VIEW: &[CommandResultCarrierKind] = &[
    CommandResultCarrierKind::Performance,
    CommandResultCarrierKind::Operation,
    CommandResultCarrierKind::ViewModel,
];
const GUARDIAN_VIEW: &[CommandResultCarrierKind] = &[
    CommandResultCarrierKind::Guardian,
    CommandResultCarrierKind::ViewModel,
];

const COMMAND_CATALOG: &[CommandCatalogEntry] = &[
    CommandCatalogEntry {
        kind: CommandKind::LaunchInstance,
        request: CommandRequestContract::LaunchInstance,
        result: CommandResultContract::LaunchInstance,
        safety_review: CommandSafetyReview::Required,
        async_operation: true,
        carriers: GUARDIAN_OPERATION_SESSION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::InstallVersion,
        request: CommandRequestContract::InstallVersion,
        result: CommandResultContract::InstallVersion,
        safety_review: CommandSafetyReview::Required,
        async_operation: true,
        carriers: GUARDIAN_OPERATION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::RepairInstance,
        request: CommandRequestContract::RepairInstance,
        result: CommandResultContract::RepairInstance,
        safety_review: CommandSafetyReview::Required,
        async_operation: true,
        carriers: GUARDIAN_OPERATION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::ApplyPerformancePlan,
        request: CommandRequestContract::ApplyPerformancePlan,
        result: CommandResultContract::ApplyPerformancePlan,
        safety_review: CommandSafetyReview::Required,
        async_operation: true,
        carriers: GUARDIAN_PERFORMANCE_OPERATION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::RefreshPerformanceRules,
        request: CommandRequestContract::RefreshPerformanceRules,
        result: CommandResultContract::RefreshPerformanceRules,
        safety_review: CommandSafetyReview::Conditional,
        async_operation: false,
        carriers: PERFORMANCE_OPERATION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::StopSession,
        request: CommandRequestContract::StopSession,
        result: CommandResultContract::StopSession,
        safety_review: CommandSafetyReview::Required,
        async_operation: false,
        carriers: GUARDIAN_OPERATION_SESSION_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::ValidateInstance,
        request: CommandRequestContract::ValidateInstance,
        result: CommandResultContract::ValidateInstance,
        safety_review: CommandSafetyReview::Required,
        async_operation: false,
        carriers: GUARDIAN_VIEW,
    },
    CommandCatalogEntry {
        kind: CommandKind::RefreshAccountReadiness,
        request: CommandRequestContract::RefreshAccountReadiness,
        result: CommandResultContract::RefreshAccountReadiness,
        safety_review: CommandSafetyReview::Conditional,
        async_operation: false,
        carriers: GUARDIAN_VIEW,
    },
];

pub fn phase_one_command_kinds() -> &'static [CommandKind] {
    PHASE_ONE_COMMAND_KINDS
}

pub fn command_catalog() -> &'static [CommandCatalogEntry] {
    COMMAND_CATALOG
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommandCatalogEntry {
    pub kind: CommandKind,
    pub request: CommandRequestContract,
    pub result: CommandResultContract,
    pub safety_review: CommandSafetyReview,
    pub async_operation: bool,
    pub carriers: &'static [CommandResultCarrierKind],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum CommandRequestContract {
    LaunchInstance,
    InstallVersion,
    RepairInstance,
    ApplyPerformancePlan,
    RefreshPerformanceRules,
    StopSession,
    ValidateInstance,
    RefreshAccountReadiness,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum CommandResultContract {
    LaunchInstance,
    InstallVersion,
    RepairInstance,
    ApplyPerformancePlan,
    RefreshPerformanceRules,
    StopSession,
    ValidateInstance,
    RefreshAccountReadiness,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum CommandSafetyReview {
    Required,
    Conditional,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum CommandResultCarrierKind {
    Guardian,
    Performance,
    Operation,
    Session,
    ViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "request")]
pub enum ApplicationCommandRequest {
    LaunchInstance(LaunchInstanceCommand),
    InstallVersion(InstallVersionCommand),
    RepairInstance(RepairInstanceCommand),
    ApplyPerformancePlan(ApplyPerformancePlanCommand),
    RefreshPerformanceRules(RefreshPerformanceRulesCommand),
    StopSession(StopSessionCommand),
    ValidateInstance(ValidateInstanceCommand),
    RefreshAccountReadiness(RefreshAccountReadinessCommand),
}

impl ApplicationCommandRequest {
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::LaunchInstance(_) => CommandKind::LaunchInstance,
            Self::InstallVersion(_) => CommandKind::InstallVersion,
            Self::RepairInstance(_) => CommandKind::RepairInstance,
            Self::ApplyPerformancePlan(_) => CommandKind::ApplyPerformancePlan,
            Self::RefreshPerformanceRules(_) => CommandKind::RefreshPerformanceRules,
            Self::StopSession(_) => CommandKind::StopSession,
            Self::ValidateInstance(_) => CommandKind::ValidateInstance,
            Self::RefreshAccountReadiness(_) => CommandKind::RefreshAccountReadiness,
        }
    }

    pub fn target(&self) -> Option<TargetDescriptor> {
        match self {
            Self::LaunchInstance(command) => Some(instance_target(command.instance_id.as_str())),
            Self::InstallVersion(command) => Some(version_target(command.version_id.as_str())),
            Self::RepairInstance(command) => command
                .target
                .clone()
                .or_else(|| Some(instance_target(command.instance_id.as_str()))),
            Self::ApplyPerformancePlan(command) => command
                .instance_id
                .as_deref()
                .map(instance_target)
                .or_else(|| Some(performance_target("performance_plan"))),
            Self::RefreshPerformanceRules(_) => Some(performance_target("performance_rules")),
            Self::StopSession(command) => Some(session_target(command.session_id.as_str())),
            Self::ValidateInstance(command) => Some(instance_target(command.instance_id.as_str())),
            Self::RefreshAccountReadiness(command) => command
                .account_id
                .as_deref()
                .map(account_target)
                .or_else(|| Some(account_target("active_account"))),
        }
    }

    pub fn command(&self) -> ApplicationCommand {
        let mut command = ApplicationCommand::new(self.kind());
        command.target = self.target();
        command
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchInstanceCommand {
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_memory_mb: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_started_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionCommand {
    pub version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairInstanceCommand {
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetDescriptor>,
    #[serde(default)]
    pub user_confirmed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplyPerformancePlanCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<PerformancePlanCommandAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
    #[serde(default)]
    pub queued: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformancePlanCommandAction {
    Install,
    Remove,
    Rollback,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefreshPerformanceRulesCommand;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StopSessionCommand {
    pub session_id: String,
    pub reason: StopSessionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopSessionReason {
    UserRequested,
    GuardianRequested,
    OperationCancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidateInstanceCommand {
    pub instance_id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefreshAccountReadinessCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub refresh_provider: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload")]
pub enum ApplicationCommandPayload {
    LaunchInstance(LaunchInstancePayload),
    InstallVersion(InstallVersionPayload),
    RepairInstance(RepairInstancePayload),
    ApplyPerformancePlan(ApplyPerformancePlanPayload),
    RefreshPerformanceRules(RefreshPerformanceRulesPayload),
    StopSession(StopSessionPayload),
    ValidateInstance(ValidateInstancePayload),
    RefreshAccountReadiness(RefreshAccountReadinessPayload),
}

impl ApplicationCommandPayload {
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::LaunchInstance(_) => CommandKind::LaunchInstance,
            Self::InstallVersion(_) => CommandKind::InstallVersion,
            Self::RepairInstance(_) => CommandKind::RepairInstance,
            Self::ApplyPerformancePlan(_) => CommandKind::ApplyPerformancePlan,
            Self::RefreshPerformanceRules(_) => CommandKind::RefreshPerformanceRules,
            Self::StopSession(_) => CommandKind::StopSession,
            Self::ValidateInstance(_) => CommandKind::ValidateInstance,
            Self::RefreshAccountReadiness(_) => CommandKind::RefreshAccountReadiness,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandPayloadStatus {
    Accepted,
    Queued,
    Running,
    Succeeded,
    Blocked,
    Failed,
    NotStarted,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchInstancePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairInstancePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplyPerformancePlanPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefreshPerformanceRulesPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StopSessionPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ValidateInstancePayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fact_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefreshAccountReadinessPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

pub fn empty_command_result(kind: CommandKind) -> CommandResult<EmptyPayload> {
    CommandResult {
        command: kind,
        operation_id: None,
        status: OperationStatus::Planned,
        safety: None,
        carriers: CommandResultCarriers::default(),
        payload: EmptyPayload,
        view_model: None,
    }
}

fn instance_target(instance_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Instance,
        instance_id,
        OwnershipClass::UserOwned,
    )
}

fn version_target(version_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Version,
        version_id,
        OwnershipClass::LauncherManaged,
    )
}

fn performance_target(target_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::PerformanceComposition,
        target_id,
        OwnershipClass::CompositionManaged,
    )
}

fn session_target(session_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Session,
        session_id,
        OwnershipClass::LauncherManaged,
    )
}

fn account_target(account_id: &str) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Application,
        TargetKind::Account,
        account_id,
        OwnershipClass::UserOwned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{
        ApplicationViewModel, BackendNotice, GuardianCommandCarrier, NoticeLevel,
        OperationCommandCarrier, PerformanceCommandCarrier, SessionCommandCarrier, ViewModelKind,
    };
    use std::collections::HashSet;

    #[test]
    fn command_catalog_covers_plan_phase_one_commands() {
        let catalog = command_catalog();
        let kinds = catalog
            .iter()
            .map(|entry| entry.kind)
            .collect::<HashSet<_>>();

        assert_eq!(catalog.len(), phase_one_command_kinds().len());
        for required in phase_one_command_kinds() {
            assert!(kinds.contains(required), "missing {required:?}");
        }
        assert!(catalog.iter().all(|entry| {
            entry.request as u8 == entry.result as u8
                && !entry.carriers.is_empty()
                && matches!(
                    entry.safety_review,
                    CommandSafetyReview::Required | CommandSafetyReview::Conditional
                )
        }));
    }

    #[test]
    fn command_requests_author_backend_targets_without_raw_path_material() {
        let request = ApplicationCommandRequest::LaunchInstance(LaunchInstanceCommand {
            instance_id: r"C:\Users\Alice\.minecraft\instances\secret".to_string(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        });

        let command = request.command();

        assert_eq!(command.kind, CommandKind::LaunchInstance);
        let target = command.target.expect("command target");
        assert_eq!(target.kind, TargetKind::Instance);
        assert_eq!(target.ownership, OwnershipClass::UserOwned);
        assert_eq!(target.id, "target");
    }

    #[test]
    fn command_payloads_keep_kind_tied_to_result_contract() {
        let payload =
            ApplicationCommandPayload::ApplyPerformancePlan(ApplyPerformancePlanPayload {
                install_id: Some("install-1".to_string()),
                operation_id: Some(OperationId::new("operation-1")),
            });

        let encoded = serde_json::to_value(&payload).expect("serialize payload");

        assert_eq!(payload.kind(), CommandKind::ApplyPerformancePlan);
        assert_eq!(encoded["command"], "ApplyPerformancePlan");
        assert_eq!(encoded["payload"]["install_id"], "install-1");
    }

    #[test]
    fn command_result_can_carry_system_outputs_without_frontend_inference() {
        let result = CommandResult {
            command: CommandKind::LaunchInstance,
            operation_id: Some(OperationId::new("operation-1")),
            status: OperationStatus::Running,
            safety: None,
            carriers: CommandResultCarriers {
                guardian: Some(GuardianCommandCarrier {
                    decision: None,
                    safety: None,
                    facts: Vec::new(),
                }),
                performance: Some(PerformanceCommandCarrier {
                    health: Some("degraded".to_string()),
                    ..PerformanceCommandCarrier::default()
                }),
                operation: Some(OperationCommandCarrier {
                    operation_id: Some(OperationId::new("operation-1")),
                    status: Some(OperationStatus::Running),
                    ..OperationCommandCarrier::default()
                }),
                session: Some(SessionCommandCarrier {
                    session_id: Some("session-1".to_string()),
                    state: Some("launching".to_string()),
                    ..SessionCommandCarrier::default()
                }),
            },
            payload: EmptyPayload,
            view_model: Some(ApplicationViewModel {
                kind: ViewModelKind::LaunchActionState,
                target: Some(session_target("session-1")),
                notices: vec![BackendNotice {
                    level: NoticeLevel::Info,
                    message: "Launch is starting.".to_string(),
                    detail: None,
                }],
                available_actions: vec![CommandKind::StopSession],
                payload: None,
            }),
        };

        let encoded = serde_json::to_string(&result).expect("serialize command result");

        assert!(encoded.contains("\"guardian\""));
        assert!(encoded.contains("\"performance\""));
        assert!(encoded.contains("\"operation\""));
        assert!(encoded.contains("\"session\""));
        assert!(encoded.contains("\"view_model\""));
        assert!(encoded.contains("Launch is starting."));
    }
}
