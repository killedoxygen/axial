//! Authority cut lines for the stabilization rewrite.
//!
//! These contracts name decision locations, target owners, and source-level
//! ownership gates used by the stabilization rewrite.

use crate::state::contracts::StabilizationSystem;

pub fn authority_cut_lines() -> &'static [AuthorityCutLine] {
    AUTHORITY_CUT_LINES
}

pub const AUTHORITY_CUT_LINES: &[AuthorityCutLine] = &[
    AuthorityCutLine {
        category: DecisionCategory::LaunchSafetyReadiness,
        current_locations: &[
            DecisionLocation::ApiRoute("apps/api/src/routes/launch/task.rs"),
            DecisionLocation::ApiRoute("apps/api/src/routes/launch/runner.rs"),
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
            DecisionLocation::ApiRoute("apps/api/src/routes/performance.rs"),
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
            DecisionLocation::ApiRoute("apps/api/src/routes/launch/runner.rs"),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityCutLine {
    pub category: DecisionCategory,
    pub current_locations: &'static [DecisionLocation],
    pub target_owner: StabilizationSystem,
    pub receiving_systems: &'static [StabilizationSystem],
    pub future_plan: &'static str,
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
    ApiRoute(&'static str),
    ApiState(&'static str),
    CoreLauncher(&'static str),
    CoreMinecraft(&'static str),
    CorePerformance(&'static str),
    CoreConfig(&'static str),
    Frontend(&'static str),
}

#[cfg(test)]
mod tests {
    use super::{DecisionCategory, authority_cut_lines};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

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
        let types = read_repo_file("frontend/src/types.ts");

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
    fn frontend_performance_card_renders_backend_health_display() {
        let source = read_repo_file("frontend/src/views/instance/overview/PerformanceCard.tsx");

        assert_contains_all(
            "frontend/src/views/instance/overview/PerformanceCard.tsx",
            &source,
            &[
                "/performance/health",
                "state.health?.view_model",
                "program.health?.display",
                "display?.memory.label",
                "display?.runtime.label",
                "display?.mode.label",
            ],
        );
        assert_absent_all(
            "frontend/src/views/instance/overview/PerformanceCard.tsx",
            &source,
            &[
                "/performance/plan",
                "performanceModeFrom",
                "loaderKeyFromVersion",
                "globalPerformanceMode",
                "config.value?.performance_mode",
                "planLoader",
                "planGameVersion",
                "memoryGb",
            ],
        );
    }

    #[test]
    fn frontend_accounts_view_uses_backend_skin_action_state() {
        let source = read_repo_file("frontend/src/views/accounts/AccountsView.tsx");

        assert_contains_all(
            "frontend/src/views/accounts/AccountsView.tsx",
            &source,
            &["status?.skin_action?.enabled"],
        );
        assert_absent_all(
            "frontend/src/views/accounts/AccountsView.tsx",
            &source,
            &["online_mode_ready"],
        );
    }

    #[test]
    fn install_routes_delegate_workflow_ownership_to_application_helpers() {
        let install_route = read_repo_file("apps/api/src/routes/install.rs");
        let loader_route = read_repo_file("apps/api/src/routes/loaders.rs");

        assert_contains_all(
            "apps/api/src/routes/install.rs",
            &install_route,
            &[
                "stage_install_version_command",
                "begin_install_operation_journal",
                "record_install_operation_progress",
                "record_install_operation_interrupted",
                "record_install_operation_guardian_evidence",
                "repair_install_artifact_corruption_with_guardian",
                "record_install_operation_guardian_repair_outcome",
                "install_guardian_repair_summary_from_journal",
            ],
        );
        assert_contains_all(
            "apps/api/src/routes/loaders.rs",
            &loader_route,
            &[
                "stage_install_version_command",
                "begin_install_operation_journal",
                "record_install_operation_progress",
                "record_install_operation_interrupted",
            ],
        );

        for (file, source) in [
            ("apps/api/src/routes/install.rs", install_route.as_str()),
            ("apps/api/src/routes/loaders.rs", loader_route.as_str()),
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
                ],
            );
        }
    }

    #[test]
    fn launch_routes_delegate_command_staging_to_application_boundary() {
        let task_route = read_repo_file("apps/api/src/routes/launch/task.rs");

        assert_contains_all(
            "apps/api/src/routes/launch/task.rs",
            &task_route,
            &["stage_launch_instance_command", "stage_launch_boundary"],
        );
        assert_absent_all(
            "apps/api/src/routes/launch/task.rs",
            &task_route,
            &[
                "ApplicationCommand {",
                "CommandResult {",
                "OperationJournalEntry::new",
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

    fn assert_contains_all(scope: &str, source: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                source.contains(needle),
                "{scope} should contain ownership marker {needle:?}"
            );
        }
    }

    fn assert_absent_all(scope: &str, source: &str, needles: &[&str]) {
        for needle in needles {
            assert!(
                !source.contains(needle),
                "{scope} must not contain decision ownership marker {needle:?}"
            );
        }
    }

    fn read_repo_file(relative: &str) -> String {
        let path = repo_root().join(relative);
        fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
    }

    fn repo_files_under(relative: &str) -> Vec<PathBuf> {
        let root = repo_root().join(relative);
        let mut files = Vec::new();
        collect_rs_files(&root, &mut files);
        files.sort();
        files
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
