//! Authority cut lines for the stabilization rewrite.
//!
//! These contracts name decision locations, target owners, and source-level
//! ownership gates used by the stabilization rewrite.

use crate::state::contracts::StabilizationSystem;

pub fn authority_cut_lines() -> &'static [AuthorityCutLine] {
    AUTHORITY_CUT_LINES
}

pub fn route_adapter_contract() -> &'static RouteAdapterContract {
    &ROUTE_ADAPTER_CONTRACT
}

pub fn route_workflow_hotspots() -> &'static [RouteWorkflowHotspot] {
    ROUTE_WORKFLOW_HOTSPOTS
}

pub fn route_boundary_probes() -> &'static [RouteBoundaryProbe] {
    ROUTE_BOUNDARY_PROBES
}

pub const AUTHORITY_CUT_LINES: &[AuthorityCutLine] = &[
    AuthorityCutLine {
        category: DecisionCategory::LaunchSafetyReadiness,
        current_locations: &[
            DecisionLocation::ApiApplication("apps/api/src/application/launch/session.rs"),
            DecisionLocation::ApiApplication("apps/api/src/application/launch/runner.rs"),
            DecisionLocation::CoreLauncher("core/launcher/src/guardian/mod.rs"),
            DecisionLocation::CoreLauncher("core/launcher/src/readiness.rs"),
        ],
        target_owner: StabilizationSystem::Guardian,
        receiving_systems: &[
            StabilizationSystem::Application,
            StabilizationSystem::Execution,
            StabilizationSystem::Observability,
            StabilizationSystem::State,
        ],
        future_plan: "05-guardian-system-and-self-healing.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::JavaRuntimeAndJvmPolicy,
        current_locations: &[
            DecisionLocation::CoreLauncher("core/launcher/src/jvm/mod.rs"),
            DecisionLocation::CoreLauncher("core/launcher/src/runtime/mod.rs"),
            DecisionLocation::CoreLauncher("core/launcher/src/service/prepare.rs"),
            DecisionLocation::CoreMinecraft("core/minecraft/src/runtime/mod.rs"),
            DecisionLocation::CoreConfig("core/config/src/instances/mod.rs"),
        ],
        target_owner: StabilizationSystem::Guardian,
        receiving_systems: &[
            StabilizationSystem::Application,
            StabilizationSystem::Execution,
            StabilizationSystem::State,
            StabilizationSystem::Observability,
        ],
        future_plan: "08-launch-runtime-session-rewrite.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::InstallDownloadIntegrity,
        current_locations: &[
            DecisionLocation::ApiRoute("apps/api/src/routes/install.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/loaders.rs"),
            DecisionLocation::CoreMinecraft("core/minecraft/src/download/mod.rs"),
            DecisionLocation::CoreMinecraft("core/minecraft/src/integrity/mod.rs"),
            DecisionLocation::CoreMinecraft("core/minecraft/src/loaders/"),
        ],
        target_owner: StabilizationSystem::Execution,
        receiving_systems: &[
            StabilizationSystem::Application,
            StabilizationSystem::Guardian,
            StabilizationSystem::Observability,
            StabilizationSystem::State,
        ],
        future_plan: "09-install-download-integrity-rewrite.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::PerformanceDecision,
        current_locations: &[
            DecisionLocation::ApiApplication("apps/api/src/application/performance.rs"),
            DecisionLocation::ApiApplication("apps/api/src/application/performance/workflow.rs"),
            DecisionLocation::CorePerformance("core/performance/src/resolve/mod.rs"),
            DecisionLocation::CorePerformance("core/performance/src/install/mod.rs"),
            DecisionLocation::CorePerformance("core/performance/src/state/mod.rs"),
        ],
        target_owner: StabilizationSystem::Performance,
        receiving_systems: &[
            StabilizationSystem::Application,
            StabilizationSystem::Guardian,
            StabilizationSystem::Execution,
            StabilizationSystem::State,
            StabilizationSystem::Observability,
        ],
        future_plan: "06-performance-system-alignment.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::SessionOutcomeClassification,
        current_locations: &[
            DecisionLocation::ApiState("apps/api/src/state/sessions/classify.rs"),
            DecisionLocation::ApiState("apps/api/src/state/sessions/mod.rs"),
            DecisionLocation::ApiApplication("apps/api/src/application/launch/runner.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/launch/stream.rs"),
        ],
        target_owner: StabilizationSystem::Guardian,
        receiving_systems: &[
            StabilizationSystem::Application,
            StabilizationSystem::Execution,
            StabilizationSystem::Observability,
            StabilizationSystem::State,
        ],
        future_plan: "08-launch-runtime-session-rewrite.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::FrontendBusinessDecision,
        current_locations: &[
            DecisionLocation::Frontend("frontend/src/launch.ts"),
            DecisionLocation::Frontend("frontend/src/launch-stages.ts"),
            DecisionLocation::Frontend("frontend/src/instance-install-status.ts"),
            DecisionLocation::Frontend("frontend/src/player-skin.ts"),
            DecisionLocation::Frontend("frontend/src/views/settings/SettingsView.tsx"),
        ],
        target_owner: StabilizationSystem::Application,
        receiving_systems: &[
            StabilizationSystem::Guardian,
            StabilizationSystem::Performance,
            StabilizationSystem::Observability,
            StabilizationSystem::Interface,
        ],
        future_plan: "10-frontend-decision-removal.md",
    },
    AuthorityCutLine {
        category: DecisionCategory::RouteWorkflowOwnership,
        current_locations: &[
            DecisionLocation::ApiRoute("apps/api/src/routes/launch/"),
            DecisionLocation::ApiRoute("apps/api/src/routes/install.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/loaders.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/performance.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/auth.rs"),
        ],
        target_owner: StabilizationSystem::Application,
        receiving_systems: &[
            StabilizationSystem::Execution,
            StabilizationSystem::Guardian,
            StabilizationSystem::Performance,
            StabilizationSystem::Observability,
            StabilizationSystem::State,
        ],
        future_plan: "07-application-commands-and-view-models.md",
    },
];

pub const ROUTE_ADAPTER_CONTRACT: RouteAdapterContract = RouteAdapterContract {
    allowed: &[
        RouteAdapterResponsibility::ParseRequest,
        RouteAdapterResponsibility::ReadTransportContext,
        RouteAdapterResponsibility::InvokeApplicationEntryPoint,
        RouteAdapterResponsibility::MapBackendResultToHttpStatus,
        RouteAdapterResponsibility::SerializeBackendAuthoredResponse,
        RouteAdapterResponsibility::StreamBackendAuthoredEvents,
    ],
    forbidden: &[
        RouteForbiddenResponsibility::SafetyPolicy,
        RouteForbiddenResponsibility::RepairPlanning,
        RouteForbiddenResponsibility::PerformanceHealthPolicy,
        RouteForbiddenResponsibility::JournalSemantics,
        RouteForbiddenResponsibility::ExitClassification,
        RouteForbiddenResponsibility::BackendFacingUserCopy,
        RouteForbiddenResponsibility::RawProviderInterpretation,
        RouteForbiddenResponsibility::RuntimeJvmReadinessPolicy,
        RouteForbiddenResponsibility::InstallRepairState,
        RouteForbiddenResponsibility::FrontendPolicy,
    ],
};

pub const ROUTE_WORKFLOW_HOTSPOTS: &[RouteWorkflowHotspot] = &[
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Launch,
        route_locations: &[
            "apps/api/src/routes/launch/mod.rs",
            "apps/api/src/routes/launch/stream.rs",
        ],
        current_route_owned_work: &[
            "launch request parsing",
            "Application launch command invocation",
            "launch stream transport adaptation",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Guardian,
            RouteHotspotOwner::Performance,
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase2LaunchRouteCutover,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Launch,
        route_locations: &["apps/api/src/routes/launch/mod.rs"],
        current_route_owned_work: &[
            "HTTP status mapping for Guardian-authored launch errors",
            "serialization of Application-authored launch responses",
        ],
        target_owner: RouteHotspotOwner::Guardian,
        supporting_owners: &[
            RouteHotspotOwner::Application,
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase2LaunchRouteCutover,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::InstallDownload,
        route_locations: &[
            "apps/api/src/routes/install.rs",
            "apps/api/src/routes/loaders.rs",
        ],
        current_route_owned_work: &[
            "Application install command invocation",
            "install stream event adaptation",
            "Application-authored install status serialization",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::Guardian,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase3InstallDownloadRouteCutover,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::InstallDownload,
        route_locations: &["apps/api/src/routes/install.rs"],
        current_route_owned_work: &[
            "HTTP status mapping for Application/Guardian-authored install results",
            "serialization of backend-authored install repair summaries",
        ],
        target_owner: RouteHotspotOwner::Guardian,
        supporting_owners: &[
            RouteHotspotOwner::Application,
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase3InstallDownloadRouteCutover,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Performance,
        route_locations: &["apps/api/src/routes/performance.rs"],
        current_route_owned_work: &[
            "HTTP request parsing for performance queries and commands",
            "Application performance command/query invocation",
            "serialization of Application-authored performance responses",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Performance,
            RouteHotspotOwner::Guardian,
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase4PerformanceRouteCutover,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::AuthAccount,
        route_locations: &[
            "apps/api/src/routes/auth.rs",
            "apps/api/src/routes/accounts.rs",
        ],
        current_route_owned_work: &[
            "HTTP request parsing for auth/account queries and commands",
            "Application auth/account command/query invocation",
            "serialization of Application-authored auth/account responses",
        ],
        target_owner: RouteHotspotOwner::Auth,
        supporting_owners: &[
            RouteHotspotOwner::Application,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Interface,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Skin,
        route_locations: &["apps/api/src/routes/skin.rs"],
        current_route_owned_work: &[
            "HTTP request parsing for skin/profile/saved-skin queries and commands",
            "Application skin command/query invocation",
            "serialization of Application-authored skin responses",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Application,
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Interface,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Version,
        route_locations: &[
            "apps/api/src/routes/versions.rs",
            "apps/api/src/routes/version_info.rs",
            "apps/api/src/routes/catalog.rs",
        ],
        current_route_owned_work: &[
            "HTTP request parsing for catalog/version queries and commands",
            "Application version/catalog command/query invocation",
            "serialization of Application-authored version/catalog responses",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Guardian,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Instance,
        route_locations: &["apps/api/src/routes/instances.rs"],
        current_route_owned_work: &[
            "HTTP request parsing for instance queries and commands",
            "Application instance command/query invocation",
            "serialization of Application-authored instance/resource responses",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::State,
            RouteHotspotOwner::Guardian,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview,
    },
    RouteWorkflowHotspot {
        area: RouteWorkflowArea::Update,
        route_locations: &["apps/api/src/routes/update.rs"],
        current_route_owned_work: &[
            "HTTP request parsing for update status",
            "Application update query invocation",
            "serialization of Application-authored update responses",
        ],
        target_owner: RouteHotspotOwner::Application,
        supporting_owners: &[
            RouteHotspotOwner::Execution,
            RouteHotspotOwner::Interface,
            RouteHotspotOwner::Observability,
        ],
        cutover_phase: RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview,
    },
];

pub const ROUTE_BOUNDARY_PROBES: &[RouteBoundaryProbe] = &[
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::SafetyPolicy,
        forbidden_markers: &[
            "decide_guardian_policy(",
            "diagnose_facts(",
            "GuardianActionPlan::new",
            "SafetyCase::new",
            "GuardianDecision { kind:",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::RuntimeJvmReadinessPolicy,
        forbidden_markers: &[
            "decide_prepare_failure(",
            "decide_startup_failure(",
            "resolve_launch_preset(",
            "summarize_launch_warnings(",
            "inspect_launch_readiness(",
            "GuardianPreflightOutcomeRequest",
            "verify_managed_runtime(",
            "execute_managed_runtime_ready_marker_repair",
            "LaunchWarningFacts",
            "PreLaunchDecision",
            "StartupFailureDecision",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::RepairPlanning,
        forbidden_markers: &[
            "GuardianRepairActionTemplate",
            "plan_launcher_managed_artifact_repair",
            "execute_guardian_artifact_repair",
            "guardian_prepare_failure_outcome(",
            "guardian_startup_failure_outcome(",
            "plan_launch_recovery_directive(",
            "record_launch_recovery_attempt(",
            "record_launch_recovery_failure(",
            "GuardianLaunchRecoveryPlanRequest",
            "GuardianLaunchRecoveryRecordRequest",
            "recovery_plan_for_startup_failure(",
            "RecoveryAction",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::ExitClassification,
        forbidden_markers: &[
            "classify_startup_failure_text(",
            "SessionOutcomeClassifier::new",
            "LaunchSessionOutcome::from_reason",
            "LaunchSessionExitReason::",
            "crashed_before_boot",
            "external_user_closed",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::JournalSemantics,
        forbidden_markers: &[
            "OperationJournalEntry::new",
            "OperationJournalStep::new",
            "RollbackState::",
            "OperationOutcome::",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::PerformanceHealthPolicy,
        forbidden_markers: &[
            "derive_health(",
            "effective_performance_plan(",
            "public_performance_notice",
            "performance_plan_summary_view_model",
            "performance_health_guardian_facts(",
            "performance_plan_guardian_facts(",
            "performance_state_error_guardian_fact(",
            "plan_performance_supervision(",
            "performance_health_proof_record(",
            "OperationJournalEntry::new",
            "OperationJournalStep::new",
            "RollbackState::",
            "PerformanceOperationPayload",
            "sanitize_operation_error",
            "ensure_installed(",
            "remove_managed(",
            "rollback_managed",
            "list_rollback_snapshots(",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::BackendFacingUserCopy,
        forbidden_markers: &[
            "Could not update managed performance files",
            "Install failed. Check your connection",
            "Could not delete the version files",
            "update check unavailable",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::RawProviderInterpretation,
        forbidden_markers: &[
            "GithubLatestRelease",
            "serde_json::from_slice::<GithubLatestRelease>",
            "AuthChainErrorKind::",
            "MicrosoftAuthErrorKind::",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::InstallRepairState,
        forbidden_markers: &[
            "InstallStore::spawn_tracked_worker_with_interrupt_handler",
            "Downloader::new",
            "install_build(",
            "prewarm_version_runtime(",
            "begin_install_operation_journal(",
            "record_install_operation_progress(",
            "record_install_operation_interrupted(",
            "record_install_operation_guardian_evidence(",
            "repair_install_artifact_corruption_with_guardian(",
            "record_install_operation_guardian_repair_outcome(",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
    RouteBoundaryProbe {
        responsibility: RouteForbiddenResponsibility::FrontendPolicy,
        forbidden_markers: &[
            "canLaunch",
            "describeFailureClass",
            "formatSelectedOnlineAuthFailure",
            "surfaceLaunchOutcome",
            "guardianNoticeDetails",
            "healingToastMessage",
        ],
        enforcement: RouteBoundaryEnforcement::EnforceNow,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityCutLine {
    pub category: DecisionCategory,
    pub current_locations: &'static [DecisionLocation],
    pub target_owner: StabilizationSystem,
    pub receiving_systems: &'static [StabilizationSystem],
    pub future_plan: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteAdapterContract {
    pub allowed: &'static [RouteAdapterResponsibility],
    pub forbidden: &'static [RouteForbiddenResponsibility],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteWorkflowHotspot {
    pub area: RouteWorkflowArea,
    pub route_locations: &'static [&'static str],
    pub current_route_owned_work: &'static [&'static str],
    pub target_owner: RouteHotspotOwner,
    pub supporting_owners: &'static [RouteHotspotOwner],
    pub cutover_phase: RouteCutoverPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteBoundaryProbe {
    pub responsibility: RouteForbiddenResponsibility,
    pub forbidden_markers: &'static [&'static str],
    pub enforcement: RouteBoundaryEnforcement,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DecisionCategory {
    LaunchSafetyReadiness,
    JavaRuntimeAndJvmPolicy,
    InstallDownloadIntegrity,
    PerformanceDecision,
    SessionOutcomeClassification,
    FrontendBusinessDecision,
    RouteWorkflowOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionLocation {
    ApiApplication(&'static str),
    ApiRoute(&'static str),
    ApiState(&'static str),
    CoreLauncher(&'static str),
    CoreMinecraft(&'static str),
    CorePerformance(&'static str),
    CoreConfig(&'static str),
    Frontend(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteAdapterResponsibility {
    ParseRequest,
    ReadTransportContext,
    InvokeApplicationEntryPoint,
    MapBackendResultToHttpStatus,
    SerializeBackendAuthoredResponse,
    StreamBackendAuthoredEvents,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteForbiddenResponsibility {
    SafetyPolicy,
    RepairPlanning,
    PerformanceHealthPolicy,
    JournalSemantics,
    ExitClassification,
    BackendFacingUserCopy,
    RawProviderInterpretation,
    RuntimeJvmReadinessPolicy,
    InstallRepairState,
    FrontendPolicy,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteWorkflowArea {
    Launch,
    InstallDownload,
    Performance,
    AuthAccount,
    Skin,
    Instance,
    Version,
    Update,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteHotspotOwner {
    Application,
    Guardian,
    Execution,
    Performance,
    State,
    Observability,
    Auth,
    Interface,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteCutoverPhase {
    Plan19Phase2LaunchRouteCutover,
    Plan19Phase3InstallDownloadRouteCutover,
    Plan19Phase4PerformanceRouteCutover,
    Plan19Phase5ResidualInterfaceDecisionReview,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RouteBoundaryEnforcement {
    EnforceNow,
    AfterCutover(RouteCutoverPhase),
}

#[cfg(test)]
mod tests {
    use super::{
        DecisionCategory, RouteAdapterResponsibility, RouteBoundaryEnforcement, RouteCutoverPhase,
        RouteForbiddenResponsibility, RouteHotspotOwner, RouteWorkflowArea, authority_cut_lines,
        route_adapter_contract, route_boundary_probes, route_workflow_hotspots,
    };
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    #[test]
    fn authority_cut_lines_cover_phase_three_categories() {
        let categories = authority_cut_lines()
            .iter()
            .map(|cut_line| cut_line.category)
            .collect::<BTreeSet<_>>();

        for required in [
            DecisionCategory::LaunchSafetyReadiness,
            DecisionCategory::JavaRuntimeAndJvmPolicy,
            DecisionCategory::InstallDownloadIntegrity,
            DecisionCategory::PerformanceDecision,
            DecisionCategory::SessionOutcomeClassification,
            DecisionCategory::FrontendBusinessDecision,
            DecisionCategory::RouteWorkflowOwnership,
        ] {
            assert!(categories.contains(&required), "missing {required:?}");
        }
    }

    #[test]
    fn route_adapter_contract_defines_adapter_responsibilities_and_forbidden_policy() {
        let contract = route_adapter_contract();
        let allowed = contract.allowed.iter().copied().collect::<BTreeSet<_>>();
        let forbidden = contract.forbidden.iter().copied().collect::<BTreeSet<_>>();

        for required in [
            RouteAdapterResponsibility::ParseRequest,
            RouteAdapterResponsibility::ReadTransportContext,
            RouteAdapterResponsibility::InvokeApplicationEntryPoint,
            RouteAdapterResponsibility::MapBackendResultToHttpStatus,
            RouteAdapterResponsibility::SerializeBackendAuthoredResponse,
            RouteAdapterResponsibility::StreamBackendAuthoredEvents,
        ] {
            assert!(allowed.contains(&required), "missing allowed {required:?}");
        }

        for prohibited in [
            RouteForbiddenResponsibility::SafetyPolicy,
            RouteForbiddenResponsibility::RepairPlanning,
            RouteForbiddenResponsibility::PerformanceHealthPolicy,
            RouteForbiddenResponsibility::JournalSemantics,
            RouteForbiddenResponsibility::ExitClassification,
            RouteForbiddenResponsibility::BackendFacingUserCopy,
            RouteForbiddenResponsibility::RawProviderInterpretation,
            RouteForbiddenResponsibility::RuntimeJvmReadinessPolicy,
            RouteForbiddenResponsibility::InstallRepairState,
            RouteForbiddenResponsibility::FrontendPolicy,
        ] {
            assert!(
                forbidden.contains(&prohibited),
                "missing forbidden {prohibited:?}"
            );
        }
    }

    #[test]
    fn route_hotspot_map_covers_current_route_workflow_areas_with_owners_and_phases() {
        let hotspots = route_workflow_hotspots();
        let areas = hotspots
            .iter()
            .map(|hotspot| hotspot.area)
            .collect::<BTreeSet<_>>();

        for required in [
            RouteWorkflowArea::Launch,
            RouteWorkflowArea::InstallDownload,
            RouteWorkflowArea::Performance,
            RouteWorkflowArea::AuthAccount,
            RouteWorkflowArea::Skin,
            RouteWorkflowArea::Instance,
            RouteWorkflowArea::Version,
            RouteWorkflowArea::Update,
        ] {
            assert!(
                areas.contains(&required),
                "missing hotspot area {required:?}"
            );
        }

        for hotspot in hotspots {
            assert!(
                !hotspot.route_locations.is_empty(),
                "{:?} hotspot must name route locations",
                hotspot.area
            );
            assert!(
                !hotspot.current_route_owned_work.is_empty(),
                "{:?} hotspot must name current route-owned work",
                hotspot.area
            );
            assert!(
                !hotspot.supporting_owners.is_empty(),
                "{:?} hotspot must name supporting owners",
                hotspot.area
            );
        }

        assert!(
            hotspots.iter().any(
                |hotspot| hotspot.target_owner == RouteHotspotOwner::Application
                    && hotspot.cutover_phase == RouteCutoverPhase::Plan19Phase2LaunchRouteCutover
            ),
            "launch route orchestration must be assigned to Application in Phase 2"
        );
        assert!(
            hotspots.iter().any(
                |hotspot| hotspot.target_owner == RouteHotspotOwner::Guardian
                    && hotspot.cutover_phase
                        == RouteCutoverPhase::Plan19Phase3InstallDownloadRouteCutover
            ),
            "install repair routing must be assigned to Guardian in Phase 3"
        );
        assert!(
            hotspots.iter().any(
                |hotspot| hotspot.target_owner == RouteHotspotOwner::Application
                    && hotspot.cutover_phase
                        == RouteCutoverPhase::Plan19Phase4PerformanceRouteCutover
                    && hotspot
                        .supporting_owners
                        .contains(&RouteHotspotOwner::Performance)
            ),
            "performance route workflow must be assigned to Application with Performance support in Phase 4"
        );
        assert!(
            hotspots
                .iter()
                .any(|hotspot| hotspot.target_owner == RouteHotspotOwner::Auth
                    && hotspot.cutover_phase
                        == RouteCutoverPhase::Plan19Phase5ResidualInterfaceDecisionReview),
            "auth/account residual route workflow must be assigned to Auth in Phase 5"
        );
    }

    #[test]
    fn route_boundary_probes_cover_forbidden_categories_and_enforce_current_cut_lines() {
        let probed = route_boundary_probes()
            .iter()
            .map(|probe| probe.responsibility)
            .collect::<BTreeSet<_>>();
        let forbidden = route_adapter_contract()
            .forbidden
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();

        for responsibility in forbidden {
            assert!(
                probed.contains(&responsibility),
                "missing route boundary probe for {responsibility:?}"
            );
        }

        let route_sources = route_source_files();
        for probe in route_boundary_probes() {
            assert_eq!(
                probe.enforcement,
                RouteBoundaryEnforcement::EnforceNow,
                "all route boundary probes must enforce now after Plan 19 cutover: {:?}",
                probe.responsibility
            );
            assert!(
                probe.forbidden_markers.len() >= 3,
                "current route boundary probe for {:?} must be category-based",
                probe.responsibility
            );
            for (display, source) in &route_sources {
                assert_absent_all(display, source, probe.forbidden_markers);
            }
        }
    }

    #[test]
    fn frontend_launch_path_only_surfaces_backend_authored_notices() {
        let source = read_repo_file("frontend/src/launch.ts");

        assert_contains_all(
            "frontend/src/launch.ts",
            &source,
            &[
                "function backendLaunchNotice",
                "function surfaceBackendLaunchNotice",
                "surfaceBackendLaunchNotice(res.notice",
                "surfaceBackendLaunchNotice(payload.notice",
                "surfaceBackendLaunchNotice(data.notice",
            ],
        );
        assert_absent_all(
            "frontend/src/launch.ts",
            &source,
            &[
                "describeFailureClass",
                "formatSelectedOnlineAuthFailure",
                "surfaceSelectedOnlineAuthFailure",
                "surfaceLaunchOutcome",
                "healingToastMessage",
                "guardianNoticeDetails",
                "failure_class",
                "java_runtime_mismatch",
                "auth_mode_incompatible",
                "startup_stalled",
            ],
        );
    }

    #[test]
    fn frontend_launch_action_uses_backend_action_state() {
        let detail = read_repo_file("frontend/src/views/instance/InstanceDetailView.tsx");
        let controls = read_repo_file("frontend/src/views/instance/components/launch.tsx");
        let launch = read_repo_file("frontend/src/launch.ts");
        let types = format!(
            "{}\n{}",
            read_repo_file("frontend/src/types-launch.ts"),
            read_repo_file("frontend/src/types-instance.ts")
        );

        assert_contains_all(
            "frontend launch action files",
            &format!("{detail}\n{controls}\n{launch}\n{types}"),
            &[
                "const launchAction = inst.launch_action",
                "launchAction={launchAction}",
                "launchAction: LaunchActionState",
                "launchAction.primary_action",
                "inst?.launch_action?.launchable",
                "launch_action: LaunchActionState",
            ],
        );
        assert_absent_all(
            "frontend launch action files",
            &format!("{detail}\n{controls}\n{launch}"),
            &["canLaunch"],
        );
    }

    #[test]
    fn frontend_settings_performance_notice_renders_backend_health_view_model() {
        let settings = read_repo_file("frontend/src/views/instance/tabs/SettingsPane.tsx");
        let helper = read_repo_file("frontend/src/views/instance/performance-mode.ts");
        let source = format!("{settings}\n{helper}");

        assert_contains_all(
            "frontend instance Settings performance notice",
            &source,
            &[
                "/performance/health",
                "fetchPerformanceHealth(inst.id)",
                "health?.view_model",
                "viewModel.tone === 'warn' || viewModel.tone === 'err'",
            ],
        );
        assert_absent_all(
            "frontend instance Settings performance notice",
            &source,
            &[
                "/performance/plan",
                "loaderKeyFromVersion",
                "planLoader",
                "planGameVersion",
            ],
        );
    }

    #[test]
    fn frontend_accounts_flow_uses_backend_auth_action_state() {
        let accounts_view = read_repo_file("frontend/src/views/accounts/AccountsView.tsx");
        let account_auth = read_repo_file("frontend/src/views/accounts/auth.ts");
        let accounts_machine = read_repo_file("frontend/src/machines/accounts.ts");
        let account_api = read_repo_file("frontend/src/views/accounts/api.ts");
        let account_switcher = read_repo_file("frontend/src/views/accounts/AccountSwitcher.tsx");
        let player_skin = read_repo_file("frontend/src/player-skin.ts");
        let passive_source = player_skin;
        let source = format!(
            "{accounts_view}\n{account_auth}\n{accounts_machine}\n{account_api}\n{account_switcher}\n{passive_source}"
        );

        assert_contains_all(
            "frontend account/auth flow",
            &source,
            &[
                "status?.skin_action",
                "skinActionsEnabled = skinAction?.enabled === true",
                "actionEnabled(account.online_action)",
                "active.online_action?.state_id",
                "actionEnabled(refreshAction)",
                "accountActionState(value.online_action)",
                "accountActionState(value.refresh_action)",
                "accountActionState(value.profile_sync_action)",
            ],
        );
        assert_absent_all(
            "frontend account/auth flow",
            &source,
            &[
                "online_mode_ready",
                "minecraft_profile_ready === true",
                "minecraft_ownership_verified === true",
                "minecraft_token_expires_in > 0",
                "msa_refresh_available === true",
            ],
        );
        assert_absent_all(
            "passive frontend account/auth loaders",
            &passive_source,
            &["/auth/refresh"],
        );
    }

    #[test]
    fn auth_account_routes_delegate_readiness_and_mutation_to_application_boundary() {
        let auth_route = read_repo_file("apps/api/src/routes/auth.rs");
        let accounts_route = read_repo_file("apps/api/src/routes/accounts.rs");
        let auth_route_production = production_rust_source(&auth_route);
        let accounts_route_production = production_rust_source(&accounts_route);
        let application_auth = read_repo_file("apps/api/src/application/auth.rs");
        let application_accounts = read_repo_file("apps/api/src/application/accounts.rs");

        assert_contains_all(
            "apps/api/src/routes/auth.rs",
            &auth_route_production,
            &[
                "application::auth_status(",
                "application::auth_refresh_for_state(",
                "application::auth_profile_sync_for_state(",
                "application::auth_logout_for_state(",
            ],
        );
        assert_contains_all(
            "apps/api/src/routes/accounts.rs",
            &accounts_route_production,
            &[
                "application::accounts(",
                "application::create_offline_account(",
                "application::patch_account(",
                "application::select_account(",
                "application::remove_account(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/auth.rs",
            &application_auth,
            &[
                "online_mode_ready",
                "AuthChainErrorKind::",
                "MicrosoftAuthErrorKind::",
                "minecraft_account_can_launch_online",
                "skin_action_state",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/accounts.rs",
            &application_accounts,
            &[
                "select_authenticated_microsoft_replacement",
                "sync_config_for_account",
                "sync_active_offline_account_from_username",
                "upsert_microsoft_account",
                "clear_pending_saved_skin_apply_for_login_id",
            ],
        );

        assert_absent_all(
            "apps/api/src/routes/auth.rs",
            &auth_route_production,
            &[
                "online_mode_ready",
                "AuthChainErrorKind::",
                "MicrosoftAuthErrorKind::",
                "minecraft_account_can_launch_online",
                "auth_refresh_error_response",
                "auth_chain_error_response",
                "upsert_microsoft_account",
                "sync_config_for_account",
            ],
        );
        assert_absent_all(
            "apps/api/src/routes/accounts.rs",
            &accounts_route_production,
            &[
                "LauncherAccountKind::",
                "validate_username",
                "offline_uuid",
                "sync_config_for_account",
                "select_authenticated_microsoft_replacement",
                "account_store_error",
                "clear_pending_saved_skin_apply_for_login_id",
            ],
        );
    }

    #[test]
    fn skin_routes_delegate_profile_saved_skin_and_provider_workflows_to_application_boundary() {
        let skin_route = read_repo_file("apps/api/src/routes/skin.rs");
        let skin_route_production = production_rust_source(&skin_route);
        let application_skin = sources_for_paths(&[
            "apps/api/src/application/skin.rs",
            "apps/api/src/application/skin",
        ]);

        assert_contains_all(
            "apps/api/src/routes/skin.rs",
            &skin_route_production,
            &[
                "application_skin::handle_skin_profile(",
                "application_skin::handle_skin_profile_reset(",
                "application_skin::handle_skin_profile_file(",
                "application_skin::handle_skin_lookup(",
                "application_skin::handle_saved_skins(",
                "application_skin::handle_save_skin(",
                "application_skin::handle_replace_saved_skin_texture(",
                "application_skin::handle_apply_saved_skin(",
                "application_skin::handle_flush_saved_skin_applies(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/skin.rs",
            &application_skin,
            &[
                "MINECRAFT_TEXTURE_URL_PREFIX",
                "sane_minecraft_texture_url",
                "normalize_skin_png",
                "skin_upload_error",
                "skin_texture_download_error",
                "PENDING_SKIN_APPLIES",
                "clear_pending_saved_skin_apply_for_login_id",
                "flush_pending_saved_skin_applies_for_launch",
            ],
        );
        assert_absent_all(
            "apps/api/src/routes/skin.rs",
            &skin_route_production,
            &[
                "MINECRAFT_TEXTURE_URL_PREFIX",
                "sane_minecraft_texture_url",
                "normalize_skin_png",
                "skin_upload_error",
                "skin_texture_download_error",
                "PENDING_SKIN_APPLIES",
                "json_error(",
                "bounded_error_message(",
                "validate_username(",
                "offline_uuid(",
            ],
        );
    }

    #[test]
    fn instance_routes_delegate_resource_workflows_to_application_boundary() {
        let instance_route = read_repo_file("apps/api/src/routes/instances.rs");
        let instance_route_production = production_rust_source(&instance_route);
        let application_instances = sources_for_paths(&[
            "apps/api/src/application/instances.rs",
            "apps/api/src/application/instances",
        ]);

        assert_contains_all(
            "apps/api/src/routes/instances.rs",
            &instance_route_production,
            &[
                "instances::handle_list_instances(",
                "instances::handle_create_instance(",
                "instances::handle_duplicate_instance(",
                "instances::handle_open_instance_folder(",
                "instances::handle_instance_resources(",
                "instances::handle_instance_worlds(",
                "instances::handle_instance_mods(",
                "instances::handle_instance_screenshots(",
                "instances::handle_instance_logs(",
                "instances::handle_delete_instance(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/instances.rs",
            &application_instances,
            &[
                "scan_current_versions",
                "instance_write_error_response",
                "resolve_instance_folder",
                "open_path(",
                "fs::rename",
                "fs::remove_dir_all",
                "fs::remove_file",
                "world_file_write_error_response",
                "mod_file_write_error_response",
                "screenshot_file_write_error_response",
            ],
        );
        assert_absent_all(
            "apps/api/src/routes/instances.rs",
            &instance_route_production,
            &[
                "scan_versions(",
                "InstanceWriteOperation",
                "instance_write_error_response",
                "resolve_instance_folder",
                "open_path(",
                "fs::rename",
                "fs::remove_dir_all",
                "fs::remove_file",
                "world_file_write_error_response",
                "mod_file_write_error_response",
                "screenshot_file_write_error_response",
                "serde_json::json!",
            ],
        );
    }

    #[test]
    fn install_routes_delegate_workflow_ownership_to_application_helpers() {
        let install_route = read_repo_file("apps/api/src/routes/install.rs");
        let loader_route = read_repo_file("apps/api/src/routes/loaders.rs");
        let install_route_production = production_rust_source(&install_route);
        let loader_route_production = production_rust_source(&loader_route);
        assert_contains_all(
            "apps/api/src/routes/install.rs",
            &install_route_production,
            &[
                "enqueue_install(",
                "InstallQueueRequest",
                "install_status(",
                "install_events_stream(",
            ],
        );
        assert_contains_all(
            "apps/api/src/routes/loaders.rs",
            &loader_route_production,
            &[
                "enqueue_install(",
                "InstallQueueRequest",
                "loader_components(",
                "loader_builds(",
                "loader_game_versions(",
                "loader_install_events_stream(",
            ],
        );
        let application_install = sources_for_paths(&[
            "apps/api/src/application/install.rs",
            "apps/api/src/application/install",
        ]);
        assert_contains_all(
            "apps/api/src/application/install",
            &application_install,
            &[
                "stage_install_version_command",
                "begin_install_operation_journal",
                "record_install_operation_progress",
                "record_install_operation_interrupted",
                "record_install_operation_guardian_evidence",
                "repair_install_artifact_corruption_with_guardian",
                "record_install_operation_guardian_repair_outcome",
                "install_guardian_repair_summary_from_journal",
                "InstallStore::spawn_tracked_worker_with_interrupt_handler",
                "Downloader::new",
                "fetch_components(",
                "fetch_builds(",
                "fetch_supported_versions(",
                "resolve_build_record(",
                "install_build(",
            ],
        );

        for (file, source) in [
            (
                "apps/api/src/routes/install.rs",
                install_route_production.as_str(),
            ),
            (
                "apps/api/src/routes/loaders.rs",
                loader_route_production.as_str(),
            ),
        ] {
            assert_absent_all(
                file,
                source,
                &[
                    "OperationJournalEntry::new",
                    "ApplicationCommand {",
                    "CommandResult {",
                    "GuardianDecision {",
                    "GuardianActionPlan::new",
                    "diagnose_facts(",
                    "plan_launcher_managed_artifact_repair",
                    "execute_guardian_artifact_repair",
                    "InstallStore::spawn_tracked_worker_with_interrupt_handler",
                    "Downloader::new",
                    "fetch_builds(",
                    "fetch_supported_versions(",
                    "fetch_components(",
                    "resolve_build_record(",
                    "install_build(",
                    "prewarm_version_runtime(",
                    "begin_install_operation_journal(",
                    "record_install_operation_progress(",
                    "record_install_operation_interrupted(",
                    "record_install_operation_guardian_evidence(",
                    "repair_install_artifact_corruption_with_guardian(",
                    "record_install_operation_guardian_repair_outcome(",
                ],
            );
        }
    }

    #[test]
    fn install_application_delegates_guardian_repair_decisions_to_guardian() {
        let repair_source = read_repo_file("apps/api/src/application/install/repair.rs");
        let repair_production = production_rust_source(&repair_source);
        let guardian_outcome =
            production_rust_source(&read_repo_file("apps/api/src/guardian/outcome.rs"));

        assert_contains_all(
            "apps/api/src/application/install/repair.rs",
            &repair_production,
            &[
                "install_artifact_failure_from_minecraft_download_fact(",
                "plan_install_artifact_failure_repair(",
                "execute_guardian_artifact_repair(",
                "execute_guardian_missing_artifact_repair(",
                "install_artifact_repair_user_outcome(",
            ],
        );
        assert_contains_all(
            "apps/api/src/guardian/outcome.rs",
            &guardian_outcome,
            &[
                "install_artifact_repair_user_outcome(",
                "Guardian repaired a launcher-managed install artifact.",
                "Guardian paused automatic install repair after repeated failure.",
                "Guardian blocked automatic install repair because it was unsafe.",
                "Guardian could not repair the launcher-managed install artifact.",
            ],
        );
        assert_absent_all(
            "apps/api/src/application/install/repair.rs",
            &repair_production,
            &[
                "GuardianDecision {",
                "GuardianActionPlan::new",
                "ActionPlanPrerequisite",
                "GuardianAction {",
                "GuardianDecisionKind::Repair",
                "GuardianActionKind::Repair",
                "install_repair_summary_copy",
                "Guardian repaired a launcher-managed install artifact.",
                "Guardian paused automatic install repair after repeated failure.",
                "Guardian blocked automatic install repair because it was unsafe.",
                "Guardian could not repair the launcher-managed install artifact.",
            ],
        );
    }

    #[test]
    fn launch_routes_delegate_workflow_to_application_launch_boundary() {
        let route = read_repo_file("apps/api/src/routes/launch/mod.rs");
        let app_session = sources_for_paths(&[
            "apps/api/src/application/launch/session.rs",
            "apps/api/src/application/launch/session",
        ]);
        let app_runner = sources_for_paths(&[
            "apps/api/src/application/launch/runner.rs",
            "apps/api/src/application/launch/runner",
        ]);

        assert_contains_all(
            "apps/api/src/routes/launch/mod.rs",
            &route,
            &[
                "use crate::application::launch as launch_app;",
                "launch_app::prepare_launch_preflight",
                "launch_app::prepare_launch_session",
                "launch_app::launch_session",
                "launch_app::launch_prepared_response_payload",
                "spawn_launch_session(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/launch/session.rs",
            &app_session,
            &[
                "stage_launch_instance_command",
                "stage_launch_boundary",
                "inspect_launch_readiness(",
                "GuardianPreflightOutcomeRequest",
                "execute_managed_runtime_ready_marker_repair",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/launch/runner.rs",
            &app_runner,
            &[
                "prepare_launch_attempt_with_events(",
                "guardian_prepare_failure_outcome(",
                "guardian_startup_failure_outcome(",
                "plan_guardian_launch_recovery_directive(",
                "record_launch_recovery_attempt(",
                "record_launch_recovery_failure(",
            ],
        );

        for path in repo_files_under("apps/api/src/routes/launch")
            .into_iter()
            .filter(|path| !is_rust_test_source(path))
        {
            let display = path.strip_prefix(repo_root()).unwrap_or(&path).display();
            let raw_source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {display}: {error}"));
            let source = production_rust_source(&raw_source);
            assert_absent_all(
                &display.to_string(),
                &source,
                &[
                    "ApplicationCommand {",
                    "CommandResult {",
                    "stage_launch_instance_command",
                    "stage_launch_boundary",
                    "inspect_launch_readiness(",
                    "GuardianPreflightOutcomeRequest",
                    "execute_managed_runtime_ready_marker_repair",
                    "guardian_prepare_failure_outcome(",
                    "guardian_startup_failure_outcome(",
                    "plan_guardian_launch_recovery_directive(",
                    "record_launch_recovery_attempt(",
                    "record_launch_recovery_failure(",
                ],
            );
        }
    }

    #[test]
    fn launch_recovery_delegates_directive_planning_to_guardian() {
        let guardian_recovery =
            production_rust_source(&read_repo_file("apps/api/src/guardian/launch_recovery.rs"));
        let guardian_outcome =
            production_rust_source(&read_repo_file("apps/api/src/guardian/outcome.rs"));
        let app_recovery = production_rust_source(&read_repo_file(
            "apps/api/src/application/launch/runner/recovery.rs",
        ));
        let app_runner =
            production_rust_source(&read_repo_file("apps/api/src/application/launch/runner.rs"));

        assert_contains_all(
            "apps/api/src/guardian/launch_recovery.rs",
            &guardian_recovery,
            &[
                "GuardianLaunchRecoveryPlan",
                "GuardianLaunchRecoveryActionTemplate",
                "GuardianLaunchRecoveryPlanRequest",
                "plan: &'a GuardianLaunchRecoveryPlan",
                "pub fn plan_launch_recovery_directive(",
                "directive_kind_matches_effect(",
            ],
        );
        assert_contains_all(
            "apps/api/src/guardian/outcome.rs",
            &guardian_outcome,
            &[
                "launch_recovery_suppressed_user_outcome(",
                "launch_recovery_public_action_label(",
                "Guardian suppressed a repeated launch self-healing retry",
                "Review the latest game log or change the affected launch setting before retrying.",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/launch/runner/recovery.rs",
            &app_recovery,
            &[
                "plan_launch_recovery_directive(",
                "GuardianLaunchRecoveryPlanRequest {",
                "GuardianLaunchRecoveryRecordRequest {",
                "plan: &GuardianLaunchRecoveryPlan",
                "launch_recovery_suppressed_user_outcome(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/launch/runner.rs",
            &app_runner,
            &[
                "let mut last_recovery_plan: Option<GuardianLaunchRecoveryPlan>",
                "plan_guardian_launch_recovery_directive(",
                "record_guardian_launch_recovery_attempt(",
                "apply_prepare_recovery_directive(&mut guardian, &mut attempt, &recovery_plan)",
                "apply_startup_recovery_directive(&mut guardian, &mut attempt, &recovery_plan)",
                "last_recovery_plan.as_ref()",
            ],
        );
        assert_absent_all(
            "apps/api/src/application/launch/runner.rs",
            &app_runner,
            &["let recovery_kind = directive.kind", "last_recovery_kind"],
        );
        assert_absent_all(
            "apps/api/src/application/launch/runner/recovery.rs",
            &app_recovery,
            &[
                "suppressed_launch_recovery_message",
                "Guardian suppressed a repeated launch self-healing retry",
                "explicit JVM argument recovery",
                "managed Java recovery",
                "JVM preset recovery",
                "custom GC flag recovery",
            ],
        );
    }

    #[test]
    fn launch_runtime_repair_delegates_guardian_planning_to_guardian() {
        let runtime_repair =
            read_repo_file("apps/api/src/application/launch/session/runtime_repair.rs");
        let runtime_repair_production = production_rust_source(&runtime_repair);

        assert_contains_all(
            "apps/api/src/application/launch/session/runtime_repair.rs",
            &runtime_repair_production,
            &[
                "stage_launch_boundary(",
                "plan_managed_runtime_ready_marker_repair(",
                "execute_managed_runtime_ready_marker_repair(",
                "runtime_repair_user_outcome(",
                "GuardianManagedRuntimeRepairRequest {",
                "plan: &repair_plan",
            ],
        );
        assert_absent_all(
            "apps/api/src/application/launch/session/runtime_repair.rs",
            &runtime_repair_production,
            &[
                "GuardianDecision {",
                "GuardianActionPlan::new",
                "ActionPlanPrerequisite",
                "GuardianAction {",
                "GuardianDecisionKind::Repair",
                "GuardianActionKind::Repair",
                "Guardian repaired launch state before launch.",
                "Guardian repaired the managed Java runtime before launch.",
                "Guardian suppressed managed Java runtime repair",
                "Guardian could not repair the managed Java runtime automatically.",
                "Guardian blocked managed Java runtime repair because it was not safe to apply.",
                "Reinstall or repair the affected version/runtime before launching again.",
            ],
        );
    }

    #[test]
    fn performance_routes_delegate_workflow_to_application_performance_boundary() {
        let route = read_repo_file("apps/api/src/routes/performance.rs");
        let route_production = production_rust_source(&route);
        let application_performance = sources_for_paths(&[
            "apps/api/src/application/performance.rs",
            "apps/api/src/application/performance",
        ]);
        let workflow = production_rust_source(&read_repo_file(
            "apps/api/src/application/performance/workflow.rs",
        ));
        let application_sources = format!("{application_performance}\n{workflow}");

        assert_contains_all(
            "apps/api/src/routes/performance.rs",
            &route_production,
            &[
                "application::performance_rules_status(",
                "application::refresh_performance_rules(",
                "application::refresh_performance_rules_error_response",
                "application::performance_plan(",
                "application::performance_health(",
                "application::performance_rollback_list(",
                "application::performance_install(",
                "application::performance_operation_status(",
                "application::performance_instance_operation(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/performance",
            &application_sources,
            &[
                "performance_plan_summary_view_model",
                "public_performance_notice",
                "derive_health(",
                "effective_performance_plan(",
                "load_state(",
                "scan_versions(",
                "performance_health_guardian_facts(",
                "performance_plan_guardian_facts(",
                "performance_state_error_guardian_fact(",
                "plan_performance_supervision(",
                "performance_health_proof_record(",
                "OperationJournalEntry::new",
                "OperationJournalStep::new",
                "PerformanceOperationPayload",
                "sanitize_operation_error",
                "ensure_installed(",
                "remove_managed(",
                "rollback_managed",
                "list_rollback_snapshots(",
            ],
        );
        assert_absent_all(
            "apps/api/src/routes/performance.rs",
            &route_production,
            &[
                "derive_health(",
                "effective_performance_plan(",
                "load_state(",
                "scan_versions(",
                "performance_health_guardian_facts(",
                "performance_plan_guardian_facts(",
                "performance_state_error_guardian_fact(",
                "performance_health_proof_record(",
                "OperationJournalEntry::new",
                "OperationJournalStep::new",
                "RollbackState::",
                "PerformanceOperationPayload",
                "sanitize_operation_error",
                "ensure_installed(",
                "remove_managed(",
                "rollback_managed",
                "list_rollback_snapshots(",
                "public_performance_notice",
                "performance_plan_summary_view_model",
                "Could not update managed performance files",
                "Could not load performance data",
            ],
        );
    }

    #[test]
    fn performance_mutation_delegates_safety_supervision_to_guardian() {
        let mutation = production_rust_source(&read_repo_file(
            "apps/api/src/application/performance/workflow/mutation.rs",
        ));
        let operations = production_rust_source(&read_repo_file(
            "apps/api/src/application/performance/workflow/operations.rs",
        ));
        let guardian_performance =
            production_rust_source(&read_repo_file("apps/api/src/guardian/performance.rs"));
        let guardian_outcome =
            production_rust_source(&read_repo_file("apps/api/src/guardian/outcome.rs"));
        let core_install =
            production_rust_source(&read_repo_file("core/performance/src/install/mutation.rs"));
        let core_resolve =
            production_rust_source(&read_repo_file("core/performance/src/resolve/planner.rs"));

        assert_contains_all(
            "apps/api/src/guardian/performance.rs",
            &guardian_performance,
            &[
                "GuardianPerformanceSupervisionRequest",
                "GuardianPerformanceSupervisionPlan",
                "plan_performance_supervision(",
                "decide_guardian_policy(",
                "build_safety_case(",
            ],
        );
        assert_contains_all(
            "apps/api/src/guardian/outcome.rs",
            &guardian_outcome,
            &[
                "performance_supervision_rejection_user_outcome(",
                "performance update was blocked by Guardian safety supervision",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/performance/workflow/mutation.rs",
            &mutation,
            &[
                "plan_performance_supervision(",
                "GuardianPerformanceSupervisionRequest",
                "GuardianPerformanceOperationKind::ApplyManagedComposition",
                "GuardianPerformanceOperationKind::RemoveManagedComposition",
                "GuardianPerformanceOperationKind::RollbackManagedComposition",
                "performance_plan_guardian_facts(",
                "performance_failure_memory_guardian_fact(",
                "record_performance_guardian_supervision(",
                "performance_supervision_rejection_user_outcome(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/performance/workflow/operations.rs",
            &operations,
            &[
                "record_performance_guardian_supervision(",
                "GuardianPerformanceSupervisionPlan",
                "record_guardian_evidence(",
            ],
        );
        assert_absent_all(
            "apps/api/src/application/performance/workflow/mutation.rs",
            &mutation,
            &[
                "GuardianDecision {",
                "GuardianActionPlan::new",
                "GuardianAction {",
                "decide_guardian_policy(",
                "build_safety_case(",
                "performance update was blocked by Guardian safety supervision",
            ],
        );
        assert_absent_all(
            "core/performance/src/install/mutation.rs",
            &core_install,
            &[
                "GuardianPerformance",
                "decide_guardian_policy(",
                "build_safety_case(",
            ],
        );
        assert_absent_all(
            "core/performance/src/resolve/planner.rs",
            &core_resolve,
            &[
                "GuardianPerformance",
                "decide_guardian_policy(",
                "build_safety_case(",
            ],
        );
    }

    #[test]
    fn update_route_delegates_provider_interpretation_to_application_boundary() {
        let route = production_rust_source(&read_repo_file("apps/api/src/routes/update.rs"));
        let application_update =
            production_rust_source(&read_repo_file("apps/api/src/application/update.rs"));

        assert_contains_all(
            "apps/api/src/routes/update.rs",
            &route,
            &["application::update_status("],
        );
        assert_contains_all(
            "apps/api/src/application/update.rs",
            &application_update,
            &[
                "GithubLatestRelease",
                "serde_json::from_slice::<GithubLatestRelease>",
                "UPDATE_CHECK_UNAVAILABLE_MESSAGE",
                "release_response_for_platform(",
                "matching_release_asset(",
            ],
        );
        assert_absent_all(
            "apps/api/src/routes/update.rs",
            &route,
            &[
                "GithubLatestRelease",
                "serde_json::from_slice::<GithubLatestRelease>",
                "UPDATE_CHECK_UNAVAILABLE_MESSAGE",
                "release_response_for_platform(",
                "matching_release_asset(",
                "update check unavailable",
            ],
        );
    }

    #[test]
    fn version_routes_delegate_scan_catalog_and_delete_workflows_to_application_boundary() {
        let versions_route =
            production_rust_source(&read_repo_file("apps/api/src/routes/versions.rs"));
        let version_info_route =
            production_rust_source(&read_repo_file("apps/api/src/routes/version_info.rs"));
        let catalog_route =
            production_rust_source(&read_repo_file("apps/api/src/routes/catalog.rs"));
        let application_version =
            production_rust_source(&read_repo_file("apps/api/src/application/version.rs"));
        let route_sources = format!("{versions_route}\n{version_info_route}\n{catalog_route}");

        assert_contains_all(
            "version/catalog route adapters",
            &route_sources,
            &[
                "application::installed_versions(",
                "application::installed_versions_event_payload(",
                "application::version_info(",
                "application::open_version_folder(",
                "application::delete_version(",
                "application::catalog(",
            ],
        );
        assert_contains_all(
            "apps/api/src/application/version.rs",
            &application_version,
            &[
                "scan_installed_versions(",
                "fetch_version_manifest_cached(",
                "analyze_minecraft_version(",
                "fs::remove_dir_all(",
                "open_path(",
                "VERSION_DELETE_ERROR_MESSAGE",
                "catalog_fetch_error_response(",
            ],
        );
        assert_absent_all(
            "version/catalog route adapters",
            &route_sources,
            &[
                "scan_versions(",
                "fetch_version_manifest_cached(",
                "analyze_minecraft_version(",
                "fs::remove_dir_all(",
                "open_path(",
                "VERSION_DELETE_ERROR_MESSAGE",
                "Could not delete the version files",
                "Could not scan installed versions",
                "Could not load the Minecraft catalog",
            ],
        );
    }

    #[test]
    fn execution_capabilities_do_not_own_guardian_decisions() {
        for path in repo_files_under("apps/api/src/execution") {
            let display = path.strip_prefix(repo_root()).unwrap_or(&path).display();
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {display}: {error}"));

            assert_absent_all(
                &display.to_string(),
                &source,
                &[
                    "crate::guardian",
                    "GuardianDecision",
                    "GuardianSummary",
                    "GuardianAction",
                    "DiagnosisId",
                    "GuardianMode",
                    "GuardianRepair",
                    "SafetyOutcome",
                    "decide_guardian",
                    "diagnose_facts(",
                    "plan_launcher_",
                    "execute_guardian_",
                    "LaunchNotice",
                ],
            );
        }
    }

    #[test]
    fn legacy_launcher_guardian_policy_is_test_only_and_not_hot_path_exported() {
        let core_guardian = read_repo_file("core/launcher/src/guardian/mod.rs");
        assert_contains_all(
            "core/launcher/src/guardian/mod.rs",
            &core_guardian,
            &[
                "#[cfg(test)]\nfn summarize_launch_warnings(",
                "#[cfg(test)]\nfn decide_prepare_failure(",
                "#[cfg(test)]\nfn decide_startup_failure(",
                "#[cfg(test)]\nfn resolve_launch_preset(",
                "#[cfg(test)]\nfn recovery_plan_for_startup_failure(",
            ],
        );

        let core_lib = read_repo_file("core/launcher/src/lib.rs");
        assert_absent_all(
            "core/launcher public exports",
            &core_lib,
            &[
                "summarize_launch_warnings",
                "decide_prepare_failure",
                "decide_startup_failure",
                "resolve_launch_preset",
                "recovery_plan_for_startup_failure",
                "PreLaunchDecision",
                "StartupFailureDecision",
                "RecoveryAction",
            ],
        );

        for relative in [
            "core/launcher/src/service/prepare.rs",
            "core/launcher/src/service/mapping.rs",
            "apps/api/src/application/launch/session.rs",
            "apps/api/src/application/launch/runner.rs",
            "apps/api/src/routes/launch/mod.rs",
        ] {
            let source = production_rust_source(&read_repo_file(relative));
            assert_absent_all(
                relative,
                &source,
                &[
                    "summarize_launch_warnings(",
                    "decide_prepare_failure(",
                    "decide_startup_failure(",
                    "resolve_launch_preset(",
                    "recovery_plan_for_startup_failure(",
                    "PreLaunchDecision",
                    "StartupFailureDecision",
                    "RecoveryAction",
                ],
            );
        }

        assert_contains_all(
            "apps/api/src/guardian/launch_decision.rs",
            &read_repo_file("apps/api/src/guardian/launch_decision.rs"),
            &[
                "guardian_prepare_failure_outcome(",
                "guardian_startup_failure_outcome(",
                "GuardianLaunchRecoveryDirective",
                "conservative_launch_recovery_preset(",
            ],
        );
    }

    #[test]
    fn vcs_source_docs_are_reproducible_and_plans_are_local_only() {
        let untracked_required = git_output(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            "apps/api/src",
            "core/launcher/src",
            "frontend/src",
            "docs",
        ]);
        assert!(
            untracked_required.trim().is_empty(),
            "required source/docs files must be tracked or staged, found untracked:\n{}",
            untracked_required
        );

        let staged_plans = git_output(&["diff", "--cached", "--name-only", "--", "plans"]);
        assert!(
            staged_plans.trim().is_empty(),
            "plans/ is local-only and must not be staged:\n{}",
            staged_plans
        );

        let ignored_plan = git_output(&[
            "check-ignore",
            "-v",
            "--",
            "plans/stabilization/execution/GOAL.md",
        ]);
        assert!(
            ignored_plan.contains("plans/"),
            "plans/ must remain ignored as the local stabilization control plane, got:\n{}",
            ignored_plan
        );
    }

    #[test]
    fn failure_scenario_proof_matrix_covers_quality_gate_scenarios() {
        let scenarios = failure_scenario_proof_matrix();
        let covered = scenarios
            .iter()
            .map(|scenario| scenario.id)
            .collect::<BTreeSet<_>>();

        for required in [
            "java_override_undefined_null_empty_missing",
            "wrong_java_major_update",
            "java_probe_failure",
            "malformed_jvm_arg_quoting",
            "unsupported_jvm_flags",
            "memory_flag_conflicts",
            "missing_client_jar",
            "missing_library",
            "corrupt_managed_artifact",
            "incomplete_install_marker",
            "interrupted_download_with_temp_files",
            "invalid_remote_performance_rules",
            "managed_composition_rollback",
            "startup_crash_before_boot",
            "startup_stall",
            "clean_external_game_close",
            "launcher_stop",
            "crash_after_boot",
            "repeated_same_failure_suppressed_by_memory",
        ] {
            assert!(
                covered.contains(required),
                "failure scenario proof matrix missing {required}"
            );
        }

        for scenario in scenarios {
            assert!(
                !scenario.owner.is_empty(),
                "{} must name an owning system",
                scenario.id
            );
            assert!(
                !scenario.proofs.is_empty(),
                "{} must name local behavior proofs",
                scenario.id
            );
            for proof in scenario.proofs {
                let source = read_repo_file(proof.file);
                assert!(
                    test_function_exists(&source, proof.test_name),
                    "{} proof {}::{} must exist as a local test",
                    scenario.id,
                    proof.file,
                    proof.test_name
                );
            }
        }
    }

    struct FailureScenarioProof {
        id: &'static str,
        owner: &'static str,
        proofs: &'static [LocalTestProof],
    }

    struct LocalTestProof {
        file: &'static str,
        test_name: &'static str,
    }

    fn failure_scenario_proof_matrix() -> &'static [FailureScenarioProof] {
        &[
            FailureScenarioProof {
                id: "java_override_undefined_null_empty_missing",
                owner: "Application + Guardian + Execution runtime",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_undefined_java_override_exposes_guardian_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_null_java_override_exposes_guardian_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_blank_instance_java_override_uses_global_override",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_bad_custom_java_override_blocks_with_guardian_fact",
                    },
                ],
            },
            FailureScenarioProof {
                id: "wrong_java_major_update",
                owner: "Execution runtime + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_wrong_java_major_override_falls_back_with_guardian_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_old_java8_update_falls_back_with_guardian_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/execution/runtime.rs",
                        test_name: "wrong_major_emits_expected_and_actual_without_java_path",
                    },
                    LocalTestProof {
                        file: "apps/api/src/execution/runtime.rs",
                        test_name: "wrong_update_emits_required_and_actual_without_java_path",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/tests.rs",
                        test_name: "execution_java_update_fact_maps_to_update_diagnosis",
                    },
                ],
            },
            FailureScenarioProof {
                id: "java_probe_failure",
                owner: "Execution runtime + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_probe_failing_java_override_falls_back_without_raw_path",
                    },
                    LocalTestProof {
                        file: "apps/api/src/execution/runtime.rs",
                        test_name: "probe_failure_emits_probe_failed_fact_without_path",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/tests.rs",
                        test_name: "execution_runtime_fact_maps_to_confirmed_runtime_diagnosis",
                    },
                ],
            },
            FailureScenarioProof {
                id: "malformed_jvm_arg_quoting",
                owner: "Execution JVM + Application launch + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/execution/jvm.rs",
                        test_name: "malformed_jvm_args_emit_parse_fact_without_raw_args",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_malformed_jvm_args_exposes_redacted_guardian_fact",
                    },
                ],
            },
            FailureScenarioProof {
                id: "unsupported_jvm_flags",
                owner: "Execution JVM + Application launch + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/execution/jvm.rs",
                        test_name: "runtime_sensitive_gc_flags_emit_unsupported_gc_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/overrides.rs",
                        test_name: "launch_preflight_unsupported_jvm_gc_flags_exposes_guardian_fact",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/launch_decision.rs",
                        test_name: "managed_prepare_jvm_unsupported_option_returns_strip_directive",
                    },
                ],
            },
            FailureScenarioProof {
                id: "memory_flag_conflicts",
                owner: "Execution JVM + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/execution/jvm.rs",
                        test_name: "memory_classpath_native_and_agent_overrides_emit_distinct_facts",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/tests.rs",
                        test_name: "execution_jvm_unsafe_fact_maps_to_unsafe_override_diagnosis",
                    },
                ],
            },
            FailureScenarioProof {
                id: "missing_client_jar",
                owner: "Application launch + Guardian preflight",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/readiness.rs",
                        test_name: "launch_preflight_readiness_reports_missing_client_jar",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/preflight.rs",
                        test_name: "missing_launch_artifact_readiness_blocks_preflight",
                    },
                ],
            },
            FailureScenarioProof {
                id: "missing_library",
                owner: "Application launch + Guardian preflight",
                proofs: &[LocalTestProof {
                    file: "apps/api/src/application/launch/session/tests/readiness.rs",
                    test_name: "launch_preflight_readiness_reports_missing_library_metadata_as_corrupt_guardian_fact",
                }],
            },
            FailureScenarioProof {
                id: "corrupt_managed_artifact",
                owner: "Application install + Guardian artifact repair + Execution download",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/install/tests.rs",
                        test_name: "install_guardian_repair_repairs_matching_checksum_failure",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/artifact_repair.rs",
                        test_name: "repairs_launcher_managed_artifact_with_sha1_source",
                    },
                    LocalTestProof {
                        file: "apps/api/src/execution/download.rs",
                        test_name: "checksum_mismatch_does_not_promote_target",
                    },
                ],
            },
            FailureScenarioProof {
                id: "incomplete_install_marker",
                owner: "Application launch + Guardian preflight",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/readiness.rs",
                        test_name: "launch_preflight_readiness_reports_incomplete_install_marker",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/session/tests/readiness.rs",
                        test_name: "prepare_launch_session_rejects_incomplete_install_without_session",
                    },
                ],
            },
            FailureScenarioProof {
                id: "interrupted_download_with_temp_files",
                owner: "Execution download + State install + API install adapter",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/execution/download.rs",
                        test_name: "interrupted_download_discards_temp_and_reports_facts",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/install/tests.rs",
                        test_name: "install_status_exposes_interrupted_install_as_redacted_terminal_state",
                    },
                ],
            },
            FailureScenarioProof {
                id: "invalid_remote_performance_rules",
                owner: "Performance + Guardian performance",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/performance/workflow/tests/rules_status.rs",
                        test_name: "status_reports_invalid_remote_rules_with_guardian_fact_and_safe_copy",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/performance.rs",
                        test_name: "invalid_rules_status_maps_to_guardian_performance_fact",
                    },
                ],
            },
            FailureScenarioProof {
                id: "managed_composition_rollback",
                owner: "Performance + State performance operations",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/application/performance/workflow/tests/install_rollback.rs",
                        test_name: "rollback_with_specific_snapshot_id_restores_older_snapshot",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/performance/workflow/tests/operations.rs",
                        test_name: "queued_rollback_without_snapshot_emits_terminal_error",
                    },
                ],
            },
            FailureScenarioProof {
                id: "startup_crash_before_boot",
                owner: "State sessions + Application launch + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/state/sessions/classify.rs",
                        test_name: "session_outcome_classifies_startup_stall_and_preboot_crash",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/runner/recovery.rs",
                        test_name: "startup_exited_blocks_with_observed_failure_guardian_summary",
                    },
                ],
            },
            FailureScenarioProof {
                id: "startup_stall",
                owner: "State sessions + Application launch + Guardian",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/state/sessions/classify.rs",
                        test_name: "session_outcome_classifies_startup_stall_and_preboot_crash",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/runner/recovery.rs",
                        test_name: "startup_stalled_blocks_with_guardian_authored_status_payload",
                    },
                ],
            },
            FailureScenarioProof {
                id: "clean_external_game_close",
                owner: "State sessions",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/state/sessions/classify.rs",
                        test_name: "session_outcome_classifies_clean_external_close_after_boot",
                    },
                    LocalTestProof {
                        file: "apps/api/src/state/sessions/mod.rs",
                        test_name: "launch_external_close_after_boot_is_classified_cleanly",
                    },
                ],
            },
            FailureScenarioProof {
                id: "launcher_stop",
                owner: "State sessions",
                proofs: &[LocalTestProof {
                    file: "apps/api/src/state/sessions/classify.rs",
                    test_name: "session_outcome_classifies_launcher_stop_separately",
                }],
            },
            FailureScenarioProof {
                id: "crash_after_boot",
                owner: "State sessions",
                proofs: &[LocalTestProof {
                    file: "apps/api/src/state/sessions/classify.rs",
                    test_name: "session_outcome_classifies_startup_failure_postboot_crash_and_unknown_exit",
                }],
            },
            FailureScenarioProof {
                id: "repeated_same_failure_suppressed_by_memory",
                owner: "Guardian + State failure memory",
                proofs: &[
                    LocalTestProof {
                        file: "apps/api/src/guardian/policy.rs",
                        test_name: "suppression_blocks_repeated_retry_loop",
                    },
                    LocalTestProof {
                        file: "apps/api/src/guardian/launch_recovery.rs",
                        test_name: "launch_recovery_attempt_is_suppressed_while_failure_window_is_active",
                    },
                    LocalTestProof {
                        file: "apps/api/src/application/launch/runner/recovery.rs",
                        test_name: "launch_recovery_memory_records_redacted_attempt_failure_and_suppression",
                    },
                ],
            },
        ]
    }

    fn assert_contains_all(scope: &str, source: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                source.contains(needle),
                "{scope} should contain ownership marker {needle:?}"
            );
        }
    }

    fn assert_absent_all(scope: &str, source: &str, needles: &[&str]) {
        let normalized_source = normalize_rust_whitespace(source);
        for needle in needles {
            let normalized_needle = normalize_rust_whitespace(needle);
            assert!(
                !source.contains(needle) && !normalized_source.contains(&normalized_needle),
                "{scope} must not contain decision ownership marker {needle:?}"
            );
        }
    }

    fn normalize_rust_whitespace(source: &str) -> String {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn test_function_exists(source: &str, test_name: &str) -> bool {
        source.contains(&format!("fn {test_name}"))
            || source.contains(&format!("async fn {test_name}"))
    }

    fn read_repo_file(relative: &str) -> String {
        let path = repo_root().join(relative);
        fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
    }

    fn sources_for_paths(relatives: &[&str]) -> String {
        let mut sources = Vec::new();
        for relative in relatives {
            let path = repo_root().join(relative);
            if path.is_dir() {
                sources.extend(
                    repo_files_under(relative)
                        .into_iter()
                        .filter(|path| !is_rust_test_source(path))
                        .map(|path| {
                            let display = path
                                .strip_prefix(repo_root())
                                .unwrap_or(&path)
                                .display()
                                .to_string();
                            read_repo_file(&display)
                        }),
                );
            } else {
                sources.push(read_repo_file(relative));
            }
        }
        sources.join("\n")
    }

    fn is_rust_test_source(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "tests.rs")
            || path
                .components()
                .any(|component| component.as_os_str() == "tests")
    }

    fn repo_files_under(relative: &str) -> Vec<PathBuf> {
        let root = repo_root().join(relative);
        let mut files = Vec::new();
        collect_rs_files(&root, &mut files);
        files.sort();
        files
    }

    fn git_output(args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_root())
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap_or_else(|error| panic!("git {args:?} returned non-utf8 stdout: {error}"))
    }

    fn route_source_files() -> Vec<(String, String)> {
        repo_files_under("apps/api/src/routes")
            .into_iter()
            .map(|path| {
                let display = path
                    .strip_prefix(repo_root())
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let raw_source = fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("failed to read {display}: {error}"));
                (display, production_rust_source(&raw_source))
            })
            .collect()
    }

    fn production_rust_source(source: &str) -> String {
        source
            .find("#[cfg(test)]")
            .map(|index| source[..index].to_string())
            .unwrap_or_else(|| source.to_string())
    }

    fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
        {
            let entry = entry.unwrap_or_else(|error| {
                panic!("failed to read entry under {}: {error}", dir.display())
            });
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("apps/api should live two levels below repo root")
            .to_path_buf()
    }
}
