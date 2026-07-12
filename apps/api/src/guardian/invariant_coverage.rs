use super::launch_decision::failure_class_matrix_decision;
use super::launch_failure_memory::{
    launch_failure_class_for_diagnosis, launch_failure_diagnosis_id,
};
use super::repair_authorization::repair_hand_coverage;
use super::rules::DIAGNOSIS_RULES;
use super::{
    DiagnosisId, GuardianActionKind, GuardianFactId, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianMode, GuardianPrepareFailureRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, guardian_fact_from_execution,
    guardian_prepare_failure_outcome, guardian_startup_failure_outcome,
    install_artifact_failure_guardian_fact, install_artifact_failure_guardian_outcome,
};
use crate::application::install::{
    LoaderGuardianFailureDisposition, loader_install_failure_disposition,
};
use crate::application::launch::readiness_guardian_facts_for_coverage;
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::evidence_text_looks_sensitive;
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_launcher::{
    LaunchFailureClass, LaunchReadiness, LaunchReadinessReason, LaunchReadinessReasonId,
    LaunchReadinessSeverity,
};
use axial_minecraft::LoaderInstallFailureKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

const SCHEMA: &str = "axial.guardian.invariant_coverage.v1";
const REGENERATE_ENV: &str = "AXIAL_REGENERATE_GUARDIAN_INVARIANT_COVERAGE";
const EXPECTED_DECISION_COUNTS: [(GuardianActionKind, usize); 5] = [
    (GuardianActionKind::Block, 160),
    (GuardianActionKind::Fallback, 20),
    (GuardianActionKind::Strip, 30),
    (GuardianActionKind::AskUser, 30),
    (GuardianActionKind::RecordOnly, 300),
];
const RESERVED_AGENT_FACTS: [GuardianFactId; 7] = [
    GuardianFactId::AgentHookFailed,
    GuardianFactId::AgentUnavailable,
    GuardianFactId::BootMilestoneReached,
    GuardianFactId::BootMilestoneOverdue,
    GuardianFactId::GcPauseStorm,
    GuardianFactId::HeapPressureCritical,
    GuardianFactId::FrameBudgetExceeded,
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvariantCoverage {
    schema: String,
    invariants: Vec<InvariantState>,
    axes: CoverageAxes,
    kernel_cells: Vec<KernelCell>,
    rules: Vec<RuleCoverage>,
    senses: Vec<SenseCoverage>,
    adapters: AdapterCoverage,
    memory_feedback: Vec<MemoryFeedbackCoverage>,
    repair_hands: Vec<RepairHandCoverage>,
    deferred_demonstrations: Vec<DeferredDemonstration>,
    known_gaps: Vec<KnownGap>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvariantState {
    id: String,
    status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoverageAxes {
    failure_classes: Vec<String>,
    phases: Vec<String>,
    modes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelCell {
    failure_class: String,
    phase: String,
    mode: String,
    diagnosis: String,
    decision: String,
    public_surface: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleCoverage {
    diagnosis: String,
    triggers: Vec<String>,
    evidence: Vec<String>,
    phases: Vec<String>,
    actions: Vec<String>,
    memory_feedback: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SenseCoverage {
    fact: String,
    availability: String,
    referenced_by_rule: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterCoverage {
    execution: Vec<AdapterCell>,
    install: Vec<AdapterCell>,
    loader: Vec<LoaderAdapterCell>,
    readiness: Vec<AdapterCell>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterCell {
    source: String,
    fact: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoaderAdapterCell {
    source: String,
    disposition: String,
    phase: Option<String>,
    evidence_kind: Option<String>,
    fact: Option<String>,
    diagnosis: Option<String>,
    target_kind: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryFeedbackCoverage {
    failure_class: String,
    diagnosis: String,
    round_trip: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairHandCoverage {
    kind: String,
    diagnosis: String,
    max_attempts: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeferredDemonstration {
    invariant: String,
    phase: String,
    status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownGap {
    invariant: String,
    boundary: String,
    missing_variants: Vec<String>,
}

#[test]
fn guardian_invariant_coverage_matches_v1_fixture() {
    let generated = generate_coverage();
    let expected = std::fs::read(fixture_path()).expect("read Guardian invariant fixture");
    let expected: InvariantCoverage =
        serde_json::from_slice(&expected).expect("parse Guardian invariant fixture");
    assert_eq!(generated, expected);
}

#[test]
#[ignore = "set AXIAL_REGENERATE_GUARDIAN_INVARIANT_COVERAGE=1 to rewrite the fixture"]
fn regenerate_guardian_invariant_coverage_fixture() {
    assert_eq!(std::env::var(REGENERATE_ENV).as_deref(), Ok("1"));
    let bytes = snapshot_bytes(&generate_coverage());
    std::fs::write(fixture_path(), bytes).expect("write Guardian invariant fixture");
}

fn generate_coverage() -> InvariantCoverage {
    assert_rule_registration();
    assert_reserved_agent_facts_are_unused();
    let kernel_cells = kernel_cells();
    assert_decision_counts(&kernel_cells);
    assert_reachable_public_copy(&kernel_cells);

    InvariantCoverage {
        schema: SCHEMA.to_string(),
        invariants: vec![
            invariant("I1", "launch_failure_matrix_and_rules_registered"),
            invariant("I2", "current_typed_hands_registered"),
            invariant("I3", "pending_prepare_failure_guidance"),
            invariant("I4", "current_hand_attempt_bounds_registered"),
            invariant("I5", "launch_failure_surfaces_bounded_and_redacted"),
            invariant("I6", "launch_failure_mapping_round_trip_only"),
            invariant("I7", "pending_loader_delegation_proof"),
            invariant("I8", "pending_phase_4_timing"),
            invariant("I9", "reserved_facts_unused_agent_demo_pending_phase_5"),
        ],
        axes: CoverageAxes {
            failure_classes: LaunchFailureClass::ALL
                .iter()
                .map(|class| class.as_str().to_string())
                .collect(),
            phases: OperationPhase::ALL.iter().map(debug_name).collect(),
            modes: GuardianMode::ALL.iter().map(debug_name).collect(),
        },
        kernel_cells,
        rules: rule_coverage(),
        senses: sense_coverage(),
        adapters: AdapterCoverage {
            execution: execution_adapter_coverage(),
            install: install_adapter_coverage(),
            loader: loader_adapter_coverage(),
            readiness: readiness_adapter_coverage(),
        },
        memory_feedback: memory_feedback_coverage(),
        repair_hands: repair_hand_coverage()
            .into_iter()
            .map(|(kind, diagnosis, max_attempts)| RepairHandCoverage {
                kind: kind.to_string(),
                diagnosis: diagnosis.as_str().to_string(),
                max_attempts,
            })
            .collect(),
        deferred_demonstrations: vec![
            DeferredDemonstration {
                invariant: "I8".to_string(),
                phase: "phase_4".to_string(),
                status: "integrity_hot_path_timing_pending".to_string(),
            },
            DeferredDemonstration {
                invariant: "I9".to_string(),
                phase: "phase_5".to_string(),
                status: "agent_fallback_execution_pending".to_string(),
            },
        ],
        known_gaps: vec![
            KnownGap {
                invariant: "I3".to_string(),
                boundary: "prepare_failure_guidance".to_string(),
                missing_variants: vec![
                    "Unknown".to_string(),
                    "OutOfMemory".to_string(),
                    "GraphicsDriverCrash".to_string(),
                    "MissingDependency".to_string(),
                    "ModTransformationFailure".to_string(),
                    "ModAttributedCrash".to_string(),
                    "ClasspathModuleConflict".to_string(),
                    "AuthModeIncompatible".to_string(),
                    "LoaderBootstrapFailure".to_string(),
                ],
            },
            KnownGap {
                invariant: "I7".to_string(),
                boundary: "loader_preoperation_and_delegated_failures".to_string(),
                missing_variants: vec![
                    "InvalidMinecraftVersion".to_string(),
                    "InvalidBuildId".to_string(),
                    "CatalogUnavailable".to_string(),
                    "CatalogStale".to_string(),
                    "BuildNotFound".to_string(),
                    "BaseInstallFailed".to_string(),
                    "ArtifactDownloadFailed".to_string(),
                ],
            },
        ],
    }
}

fn invariant(id: &str, status: &str) -> InvariantState {
    InvariantState {
        id: id.to_string(),
        status: status.to_string(),
    }
}

fn kernel_cells() -> Vec<KernelCell> {
    let mut cells = Vec::new();
    for &failure_class in LaunchFailureClass::ALL {
        for &phase in OperationPhase::ALL {
            for &mode in GuardianMode::ALL {
                let decision = failure_class_matrix_decision(failure_class, phase, mode);
                let diagnosis = decision
                    .action_plan
                    .as_ref()
                    .map(|plan| plan.prerequisite.diagnosis_id)
                    .or_else(|| decision.diagnoses.first().copied())
                    .expect("total kernel decision has a diagnosis");
                cells.push(KernelCell {
                    failure_class: failure_class.as_str().to_string(),
                    phase: debug_name(&phase),
                    mode: debug_name(&mode),
                    diagnosis: diagnosis.as_str().to_string(),
                    decision: debug_name(&decision.kind),
                    public_surface: matches!(
                        phase,
                        OperationPhase::Preparing | OperationPhase::Launching
                    ),
                });
            }
        }
    }
    assert_eq!(cells.len(), 540);
    cells
}

fn assert_decision_counts(cells: &[KernelCell]) {
    let mut counts = BTreeMap::new();
    for cell in cells {
        *counts.entry(cell.decision.as_str()).or_insert(0_usize) += 1;
    }
    for (decision, expected) in EXPECTED_DECISION_COUNTS {
        assert_eq!(counts.get(debug_name(&decision).as_str()), Some(&expected));
    }
    assert_eq!(counts.values().sum::<usize>(), 540);
}

fn assert_reachable_public_copy(cells: &[KernelCell]) {
    for &failure_class in LaunchFailureClass::ALL {
        for &mode in GuardianMode::ALL {
            let prepare = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
                mode,
                failure_class,
                public_error: "Launch preparation failed.",
                requested_java_present: false,
                explicit_java_override_present: false,
                explicit_jvm_args_present: false,
                runtime_intervention_applied: true,
                raw_jvm_args_intervention_applied: true,
            });
            assert_public_outcome(
                prepare.user_outcome.summary(),
                prepare.user_outcome.details(),
                prepare.user_outcome.guidance(),
            );
            assert_surface_cell(
                cells,
                failure_class,
                OperationPhase::Preparing,
                mode,
                prepare.guardian_decision.kind,
            );

            let observation = if failure_class == LaunchFailureClass::StartupStalled {
                GuardianStartupFailureObservation::Stalled
            } else {
                GuardianStartupFailureObservation::Exited { failure_class }
            };
            let startup = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                mode,
                observation,
                crash_evidence: None,
                target_version_id: "1.21.5",
                runtime_major: 21,
                requested_java_present: false,
                explicit_java_override_present: false,
                explicit_jvm_args_present: false,
                explicit_jvm_preset_present: false,
                startup_recovery_applied: true,
                disable_custom_gc: true,
                effective_preset: "performance",
            });
            assert_public_outcome(
                startup.user_outcome.summary(),
                startup.user_outcome.details(),
                startup.user_outcome.guidance(),
            );
            assert_surface_cell(
                cells,
                failure_class,
                OperationPhase::Launching,
                mode,
                startup.guardian_decision.kind,
            );
        }
    }
}

fn assert_public_outcome(summary: &str, details: &[String], guidance: &[String]) {
    assert!(!summary.is_empty() && summary.len() <= 180);
    assert!(!evidence_text_looks_sensitive(summary));
    assert!(details.len() <= 6);
    assert!(guidance.len() <= 6);
    assert!(
        details
            .iter()
            .chain(guidance)
            .all(|line| !line.is_empty() && line.len() <= 240)
    );
    assert!(
        details
            .iter()
            .chain(guidance)
            .all(|line| !evidence_text_looks_sensitive(line))
    );
}

fn assert_surface_cell(
    cells: &[KernelCell],
    failure_class: LaunchFailureClass,
    phase: OperationPhase,
    mode: GuardianMode,
    decision: GuardianActionKind,
) {
    let cell = cells
        .iter()
        .find(|cell| {
            cell.failure_class == failure_class.as_str()
                && cell.phase == debug_name(&phase)
                && cell.mode == debug_name(&mode)
        })
        .expect("surface cell");
    assert!(cell.public_surface);
    assert_eq!(cell.decision, debug_name(&decision));
}

fn assert_rule_registration() {
    let mut ids = HashSet::new();
    for rule in DIAGNOSIS_RULES {
        assert!(ids.insert(rule.id));
        assert!(!rule.trigger_fact_ids.is_empty());
        assert!(!rule.candidate_actions.is_empty());
        assert!(
            rule.trigger_fact_ids
                .iter()
                .all(guardian_fact_is_registered)
        );
        assert!(
            rule.evidence_fact_ids
                .iter()
                .all(guardian_fact_is_registered)
        );
    }
}

fn assert_reserved_agent_facts_are_unused() {
    for rule in DIAGNOSIS_RULES {
        let references_reserved = rule
            .trigger_fact_ids
            .iter()
            .chain(rule.evidence_fact_ids)
            .chain(rule.required_conditions)
            .chain(
                rule.suppressions
                    .iter()
                    .flat_map(|item| item.required_conditions),
            )
            .chain(
                rule.clauses
                    .iter()
                    .flat_map(|item| item.required_conditions),
            )
            .chain(
                rule.clauses
                    .iter()
                    .flat_map(|item| item.evidence_fact_ids.into_iter().flatten()),
            )
            .any(|fact| RESERVED_AGENT_FACTS.contains(fact));
        assert!(
            !references_reserved,
            "agent fact reached current rule {}",
            rule.id
        );
    }
}

fn rule_coverage() -> Vec<RuleCoverage> {
    DIAGNOSIS_RULES
        .iter()
        .map(|rule| RuleCoverage {
            diagnosis: rule.id.as_str().to_string(),
            triggers: rule.trigger_fact_ids.iter().map(fact_name).collect(),
            evidence: rule.evidence_fact_ids.iter().map(fact_name).collect(),
            phases: rule.active_phases.iter().map(debug_name).collect(),
            actions: rule.candidate_actions.iter().map(debug_name).collect(),
            memory_feedback: rule.trigger_fact_ids.iter().any(|fact| {
                matches!(
                    fact,
                    GuardianFactId::RecentStartupFailure
                        | GuardianFactId::RecentRepairFailed
                        | GuardianFactId::RepairSuppressedUntil
                        | GuardianFactId::PerformanceRepeatedFailureMemory
                )
            }),
        })
        .collect()
}

fn sense_coverage() -> Vec<SenseCoverage> {
    GuardianFactId::ALL
        .iter()
        .map(|fact| SenseCoverage {
            fact: fact_name(fact),
            availability: if RESERVED_AGENT_FACTS.contains(fact) {
                "reserved_phase_5".to_string()
            } else {
                "registered_current".to_string()
            },
            referenced_by_rule: DIAGNOSIS_RULES
                .iter()
                .any(|rule| rule_references_fact(rule, fact)),
        })
        .collect()
}

fn rule_references_fact(rule: &super::rules::DiagnosisRule, fact: &GuardianFactId) -> bool {
    rule.trigger_fact_ids.contains(fact)
        || rule.evidence_fact_ids.contains(fact)
        || rule.required_conditions.contains(fact)
        || rule
            .suppressions
            .iter()
            .any(|item| item.required_conditions.contains(fact))
        || rule.clauses.iter().any(|item| {
            item.required_conditions.contains(fact)
                || item
                    .evidence_fact_ids
                    .is_some_and(|evidence| evidence.contains(fact))
        })
}

fn execution_adapter_coverage() -> Vec<AdapterCell> {
    ExecutionFactKind::ALL
        .iter()
        .map(|kind| {
            let fact = guardian_fact_from_execution(
                &ExecutionFact {
                    operation_id: None,
                    kind: *kind,
                    target: Some(TargetDescriptor::new(
                        StabilizationSystem::Execution,
                        TargetKind::Artifact,
                        "coverage_target",
                        OwnershipClass::LauncherManaged,
                    )),
                    fields: Vec::new(),
                },
                OperationPhase::Validating,
            );
            assert!(guardian_fact_is_registered(&fact.id));
            AdapterCell {
                source: debug_name(kind),
                fact: fact_name(&fact.id),
            }
        })
        .collect()
}

fn install_adapter_coverage() -> Vec<AdapterCell> {
    GuardianInstallArtifactFailureKind::ALL
        .iter()
        .map(|kind| {
            let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "coverage_artifact",
                *kind,
            );
            let fact =
                install_artifact_failure_guardian_fact(&evidence, OperationPhase::Installing);
            assert!(guardian_fact_is_registered(&fact.id));
            AdapterCell {
                source: debug_name(kind),
                fact: fact_name(&fact.id),
            }
        })
        .collect()
}

fn loader_adapter_coverage() -> Vec<LoaderAdapterCell> {
    LoaderInstallFailureKind::ALL
        .iter()
        .map(|failure_kind| {
            let source = debug_name(failure_kind);
            match loader_install_failure_disposition(*failure_kind) {
                LoaderGuardianFailureDisposition::PreOperation => LoaderAdapterCell {
                    source,
                    disposition: "pre_operation".to_string(),
                    phase: None,
                    evidence_kind: None,
                    fact: None,
                    diagnosis: None,
                    target_kind: None,
                },
                LoaderGuardianFailureDisposition::DelegatedBaseInstall => LoaderAdapterCell {
                    source,
                    disposition: "delegated_base_install".to_string(),
                    phase: None,
                    evidence_kind: None,
                    fact: None,
                    diagnosis: None,
                    target_kind: None,
                },
                LoaderGuardianFailureDisposition::DelegatedArtifactDownload => LoaderAdapterCell {
                    source,
                    disposition: "delegated_artifact_download".to_string(),
                    phase: None,
                    evidence_kind: None,
                    fact: None,
                    diagnosis: None,
                    target_kind: None,
                },
                LoaderGuardianFailureDisposition::Evidence {
                    kind,
                    ownership,
                    phase,
                } => {
                    let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                        None,
                        "coverage_loader_version",
                        kind,
                    )
                    .with_ownership(ownership)
                    .with_field("failure_kind", failure_kind.as_str());
                    let fact = install_artifact_failure_guardian_fact(&evidence, phase);
                    let outcome = install_artifact_failure_guardian_outcome(
                        None,
                        GuardianMode::Managed,
                        phase,
                        &[evidence],
                    )
                    .expect("loader evidence reaches a blocking Guardian outcome");
                    assert_public_outcome(
                        outcome.user_outcome.summary(),
                        outcome.user_outcome.details(),
                        outcome.user_outcome.guidance(),
                    );
                    assert!(guardian_fact_is_registered(&fact.id));
                    assert!(DiagnosisId::ALL.contains(&outcome.diagnosis_id));
                    LoaderAdapterCell {
                        source,
                        disposition: "guardian_evidence".to_string(),
                        phase: Some(debug_name(&phase)),
                        evidence_kind: Some(debug_name(&kind)),
                        fact: Some(fact_name(&fact.id)),
                        diagnosis: Some(outcome.diagnosis_id.as_str().to_string()),
                        target_kind: fact.target.as_ref().map(|target| debug_name(&target.kind)),
                    }
                }
            }
        })
        .collect()
}

fn readiness_adapter_coverage() -> Vec<AdapterCell> {
    LaunchReadinessReasonId::ALL
        .iter()
        .map(|reason| {
            let facts = readiness_guardian_facts_for_coverage(&LaunchReadiness {
                launchable: false,
                reasons: vec![LaunchReadinessReason {
                    id: *reason,
                    severity: LaunchReadinessSeverity::Blocking,
                    message: "coverage",
                }],
            });
            assert_eq!(facts.len(), 1);
            assert!(guardian_fact_is_registered(&facts[0].id));
            AdapterCell {
                source: debug_name(reason),
                fact: fact_name(&facts[0].id),
            }
        })
        .collect()
}

fn memory_feedback_coverage() -> Vec<MemoryFeedbackCoverage> {
    LaunchFailureClass::ALL
        .iter()
        .map(|class| {
            let diagnosis = launch_failure_diagnosis_id(*class);
            MemoryFeedbackCoverage {
                failure_class: class.as_str().to_string(),
                diagnosis: diagnosis.as_str().to_string(),
                round_trip: launch_failure_class_for_diagnosis(diagnosis) == Some(*class),
            }
        })
        .collect()
}

fn guardian_fact_is_registered(fact: &GuardianFactId) -> bool {
    GuardianFactId::ALL.contains(fact)
}

fn fact_name(fact: &GuardianFactId) -> String {
    fact.as_str().to_string()
}

fn debug_name<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}")
}

fn snapshot_bytes(snapshot: &InvariantCoverage) -> Vec<u8> {
    let pretty = serde_json::to_string_pretty(snapshot).expect("serialize invariant coverage");
    format!("{pretty}\n").into_bytes()
}

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-invariant-coverage-v1.json")
}
