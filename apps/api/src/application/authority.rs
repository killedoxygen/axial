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
            "diagnose(",
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
            "inspect_launch_readiness(",
            "GuardianPreflightOutcomeRequest",
            "GuardianDirective::",
            "verify_managed_runtime(",
            "execute_managed_runtime_ready_marker_repair",
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
            "GuardianCopyRequest::prepare_failure(",
            "GuardianCopyRequest::startup_failure(",
            "GuardianCopyRequest::observed_launch_failure(",
            "GuardianCopyRequest::launch_recovery_suppressed(",
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
    use std::fs;
    use std::path::{Path, PathBuf};

    const MANAGED_OWNER: &str = "apps/api/src/state/performance_managed.rs";
    const MANAGED_CLAIM: &str = "apps/api/src/state/performance_rules.rs";

    #[test]
    fn managed_composition_production_sources_cross_the_state_authority() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("api crate must live under apps/ in the repository");
        let source_root = repo_root.join("apps/api/src");
        let mut sources = Vec::new();
        collect_rust_sources(&source_root, &mut sources);
        sources.sort();

        let mut violations = Vec::new();
        for path in sources {
            let relative = path
                .strip_prefix(repo_root)
                .expect("API source must be inside the repository")
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_source(&path) || relative == "apps/api/src/application/authority.rs" {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read API source");
            let production = without_trailing_test_module(&source);
            let compact = production
                .chars()
                .filter(|value| !value.is_whitespace())
                .collect::<String>();

            for marker in [
                "load_state",
                "save_state",
                "remove_state",
                "load_rollback_snapshot",
                "save_rollback_snapshot",
                "restore_rollback_snapshot",
                "derive_health",
                "derive_health_async",
            ] {
                if contains_call(production, marker) {
                    violations.push(format!("{relative}: {marker}("));
                }
            }
            if compact.contains("axial_performance::state::") {
                violations.push(format!("{relative}: axial_performance::state::"));
            }

            let performance_source = relative.starts_with("apps/api/src/application/performance/")
                || relative == "apps/api/src/routes/performance.rs";
            if performance_source {
                for marker in ["game_dir(", ".join(\"mods\")"] {
                    if compact.contains(marker) {
                        violations.push(format!("{relative}: {marker}"));
                    }
                }
            }

            if relative != MANAGED_OWNER {
                for marker in [
                    "ManagedCompositionAuthority",
                    "ManagedInstanceIdentity",
                    ".authority.identify(",
                    ".authority.inspect(",
                    ".authority.resolve_and_inspect(",
                    ".authority.ensure_installed(",
                    ".authority.remove_managed(",
                    ".authority.rollback_managed(",
                    ".authority.rollback_managed_snapshot(",
                ] {
                    if compact.contains(marker) {
                        violations.push(format!("{relative}: {marker}"));
                    }
                }
            }
            if relative != MANAGED_CLAIM && compact.contains(".claim_managed_authority(") {
                violations.push(format!("{relative}: .claim_managed_authority("));
            }
        }

        assert!(
            violations.is_empty(),
            "managed composition authority bypasses:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn guardian_diagnosis_authority_has_no_legacy_symbols() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("api crate must live under apps/ in the repository");
        let guardian_root = repo_root.join("apps/api/src/guardian");
        let mut sources = Vec::new();
        collect_rust_sources(&guardian_root, &mut sources);
        sources.sort();

        let mut violations = Vec::new();
        for path in sources {
            if is_test_source(&path) {
                continue;
            }
            let relative = path
                .strip_prefix(repo_root)
                .expect("Guardian source must be inside the repository")
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path).expect("read Guardian source");
            let production = without_trailing_test_module(&source);

            for marker in [
                concat!("diagnose_", "facts"),
                concat!("Diagnosis", "Data"),
                concat!("Guardian", "ImpactVector"),
                concat!("decision_", "pressure_score"),
                concat!("action_", "safety_score"),
            ] {
                if production.contains(marker) {
                    violations.push(format!("{relative}: {marker}"));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "Guardian diagnosis authority regressions:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn guardian_policy_is_the_only_production_decision_constructor() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("api crate must live under apps/ in the repository");
        let guardian_root = repo_root.join("apps/api/src/guardian");
        let policy_path = "apps/api/src/guardian/policy.rs";
        let mut sources = Vec::new();
        collect_rust_sources(&guardian_root, &mut sources);
        sources.sort();

        let marker = concat!("Guardian", "Decision{");
        let declaration = concat!("pubstructGuardian", "Decision{");
        let mut violations = Vec::new();
        for path in sources {
            if is_test_source(&path) {
                continue;
            }
            let relative = path
                .strip_prefix(repo_root)
                .expect("Guardian source must be inside the repository")
                .to_string_lossy()
                .replace('\\', "/");
            if relative == policy_path {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read Guardian source");
            let production = without_trailing_test_module(&source);
            let mut compact = production
                .chars()
                .filter(|value| !value.is_whitespace())
                .collect::<String>();
            if relative == "apps/api/src/guardian/model.rs" {
                let declaration_offset = compact
                    .find(declaration)
                    .expect("GuardianDecision model declaration");
                compact.replace_range(
                    declaration_offset..declaration_offset + declaration.len(),
                    "",
                );
            }
            if compact.contains(marker) {
                violations.push(relative);
            }
        }

        assert!(
            violations.is_empty(),
            "GuardianDecision production constructors outside policy.rs:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn guardian_copy_is_the_only_production_user_outcome_authority() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("api crate must live under apps/ in the repository");
        let api_root = repo_root.join("apps/api/src");
        let copy_path = "apps/api/src/guardian/copy.rs";
        let mut sources = Vec::new();
        collect_rust_sources(&api_root, &mut sources);
        sources.sort();

        let mut violations = Vec::new();
        for path in sources {
            if is_test_source(&path) {
                continue;
            }
            let relative = path
                .strip_prefix(repo_root)
                .expect("API source must be inside the repository")
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path).expect("read API source");
            let production = without_trailing_test_module(&source);
            let compact = production
                .chars()
                .filter(|value| !value.is_whitespace())
                .collect::<String>();

            if relative != copy_path && compact.contains(concat!("GuardianUser", "Outcome{")) {
                violations.push(format!("{relative}: GuardianUserOutcome literal"));
            }
            for marker in [
                concat!(".user_outcome.", "decision="),
                concat!(".user_outcome.", "phase="),
                concat!(".user_outcome.", "summary="),
                concat!(".user_outcome.", "details="),
                concat!(".user_outcome.", "guidance="),
            ] {
                if compact.contains(marker) {
                    violations.push(format!("{relative}: {marker}"));
                }
            }
            for marker in [
                concat!("launcher_guardian_", "public_lines"),
                concat!("block_preflight_outcome_", "for_runtime_repair"),
                concat!("block_guardian_with_", "user_outcome"),
                concat!("block_guardian_for_", "suppressed_launch_recovery"),
                concat!("guardian_observed_launch_", "failure_outcome"),
                concat!("launch_recovery_suppressed_", "user_outcome"),
            ] {
                if production.contains(marker) {
                    violations.push(format!("{relative}: {marker}"));
                }
            }
            if relative != copy_path
                && production.contains(concat!(
                    "Guardian recorded failed launch ",
                    "self-healing for"
                ))
            {
                violations.push(format!("{relative}: failed-healing public copy"));
            }
        }

        assert!(
            violations.is_empty(),
            "Guardian user-outcome authority regressions:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn guardian_preset_and_launch_stage_copy_have_one_production_author() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("api crate must live under apps/ in the repository");
        let copy = fs::read_to_string(repo_root.join("apps/api/src/guardian/copy.rs"))
            .expect("read Guardian copy authority");
        for marker in [
            "Guardian adjusted the JVM preset",
            "Guardian reset an unknown JVM preset to Automatic",
            "Guardian recorded the launch safety decision.",
        ] {
            assert!(
                copy.contains(marker),
                "central copy authority is missing {marker:?}"
            );
        }

        let api_root = repo_root.join("apps/api/src");
        let copy_path = "apps/api/src/guardian/copy.rs";
        let mut sources = Vec::new();
        collect_rust_sources(&api_root, &mut sources);
        sources.sort();
        let mut violations = Vec::new();
        for path in sources {
            if is_test_source(&path)
                || path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.ends_with("_snapshot.rs"))
            {
                continue;
            }
            let relative = path
                .strip_prefix(repo_root)
                .expect("API source must be inside the repository")
                .to_string_lossy()
                .replace('\\', "/");
            if relative == copy_path {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read API source");
            let production = without_trailing_test_module(&source);
            for marker in [
                "Guardian adjusted the JVM preset",
                "Guardian reset an unknown JVM preset to Automatic",
                "Guardian recorded the launch safety decision.",
                "guardian_launch_safety_decision",
            ] {
                if production.contains(marker) {
                    violations.push(format!("{relative}: {marker}"));
                }
            }
        }
        let preset_source =
            fs::read_to_string(repo_root.join("apps/api/src/guardian/jvm_preset.rs"))
                .expect("read Guardian JVM preset source");
        let preset_production = without_trailing_test_module(&preset_source);
        for marker in [
            "PresetCatalogEntry",
            "preset_catalog",
            "GuardianJvmPresetOption",
            "label:",
            "detail:",
        ] {
            if preset_production.contains(marker) {
                violations.push(format!("apps/api/src/guardian/jvm_preset.rs: {marker}"));
            }
        }
        assert!(
            violations.is_empty(),
            "displaced Guardian copy authors:\n{}",
            violations.join("\n")
        );
    }

    fn collect_rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("read API source directory") {
            let entry = entry.expect("read API source entry");
            let file_type = entry.file_type().expect("read API source entry type");
            if file_type.is_dir() {
                collect_rust_sources(&entry.path(), sources);
            } else if file_type.is_file()
                && entry.path().extension().is_some_and(|value| value == "rs")
            {
                sources.push(entry.path());
            }
        }
    }

    fn is_test_source(path: &Path) -> bool {
        path.file_name().is_some_and(|value| value == "tests.rs")
            || path.components().any(|part| part.as_os_str() == "tests")
    }

    fn without_trailing_test_module(source: &str) -> &str {
        let mut search_start = 0;
        while let Some(offset) = source[search_start..].find("#[cfg(test)]") {
            let attribute_start = search_start + offset;
            let after_attribute = &source[attribute_start + "#[cfg(test)]".len()..];
            let item = after_attribute.trim_start();
            if item.starts_with("mod tests {") || item.starts_with("mod tests;") {
                return &source[..attribute_start];
            }
            search_start = attribute_start + "#[cfg(test)]".len();
        }
        source
    }

    fn contains_call(source: &str, name: &str) -> bool {
        source.match_indices(name).any(|(offset, _)| {
            let before_is_identifier = source[..offset]
                .chars()
                .next_back()
                .is_some_and(|value| value.is_ascii_alphanumeric() || value == '_');
            let after = source[offset + name.len()..].trim_start();
            !before_is_identifier && after.starts_with('(')
        })
    }
}
