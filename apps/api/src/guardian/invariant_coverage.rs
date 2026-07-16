use super::assess_install_artifact_failure;
use super::launch_decision::failure_class_matrix_decision;
use super::persisted_state_repair::{
    PERSISTED_STATE_REPAIR_CANDIDATES, persisted_state_startup_policy_cells,
};
use super::repair_authorization::repair_hand_coverage;
use super::rules::DIAGNOSIS_RULES;
use super::{
    DiagnosisId, GuardianActionKind, GuardianFactId, GuardianInstallArtifactFailureEvidence,
    GuardianInstallArtifactFailureKind, GuardianMode, GuardianPrepareFailureRequest,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, guardian_fact_from_execution,
    guardian_prepare_failure_outcome, guardian_startup_failure_outcome,
    install_artifact_failure_guardian_fact,
};
use crate::application::install::loader_install_guardian_evidence_kind;
use crate::application::launch::readiness_guardian_facts_for_coverage;
use crate::application::timing::{
    INTEGRITY_TIER0_CEILING_MS, LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL, LaunchPreflightSenseId,
};
use crate::execution::{ExecutionFact, ExecutionFactKind, ExecutionFactSemantics};
use crate::observability::evidence_text_looks_sensitive;
use crate::state::contracts::{
    OperationPhase, OwnershipClass, ReconciliationRung, StabilizationSystem, TargetDescriptor,
    TargetKind,
};
use crate::state::{persisted_state_repair_hand_coverage, reconciliation_hand_coverage};
use axial_launcher::{
    LaunchFailureClass, LaunchReadiness, LaunchReadinessReason, LaunchReadinessReasonId,
    LaunchReadinessSeverity,
};
use axial_minecraft::{LoaderInstallFailureKind, LoaderPreOperationFailureKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

const SCHEMA: &str = "axial.guardian.invariant_coverage.v4";
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
    persisted_state_startup_cells: Vec<PersistedStateStartupCell>,
    persisted_state_repair_hands: Vec<PersistedStateRepairHandCoverage>,
    rules: Vec<RuleCoverage>,
    facts: Vec<FactCoverage>,
    preflight_senses: Vec<PreflightSenseCoverage>,
    adapters: AdapterCoverage,
    repair_hands: Vec<RepairHandCoverage>,
    reconciliation_hands: Vec<ReconciliationHandCoverage>,
    deferred_demonstrations: Vec<DeferredDemonstration>,
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
struct PersistedStateStartupCell {
    phase: String,
    mode: String,
    diagnosis: String,
    candidate_actions: Vec<String>,
    decision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedStateRepairHandCoverage {
    admission_type: String,
    attempt_type: String,
    terminal_type: String,
    phase: String,
    mode: String,
    diagnosis: String,
    stable_key_dimensions: Vec<String>,
    max_attempts_per_stable_key_per_suppression_window: usize,
    suppression_window_hours: i64,
    operation_journal_schema: String,
    failure_memory_schema: String,
    terminal_outcomes: Vec<String>,
    durability_order: Vec<String>,
    restart_contracts: Vec<String>,
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
struct FactCoverage {
    fact: String,
    availability: String,
    referenced_by_rule: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightSenseCoverage {
    id: String,
    declared_cost_class: String,
    timing_signal: String,
    measurement_status: String,
    ceiling_ms: Option<u64>,
    evidence: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterCoverage {
    execution: Vec<ExecutionAdapterCell>,
    install: Vec<AdapterCell>,
    loader_active_install: Vec<LoaderActiveInstallAdapterCell>,
    loader_pre_operation: Vec<LoaderBoundaryAdapterCell>,
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
struct ExecutionAdapterCell {
    source: String,
    classification: String,
    fact: String,
    diagnoses: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoaderActiveInstallAdapterCell {
    source: String,
    phase: String,
    evidence_kind: String,
    fact: String,
    diagnosis: String,
    target_kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoaderBoundaryAdapterCell {
    source: String,
    boundary: String,
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
struct ReconciliationHandCoverage {
    admission_type: String,
    rung: String,
    max_attempts_per_suppression_window: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeferredDemonstration {
    invariant: String,
    phase: String,
    status: String,
}

#[test]
fn guardian_invariant_coverage_artifacts_match_v4() {
    let generated = generate_coverage();
    let expected_json =
        std::fs::read(json_fixture_path()).expect("read Guardian invariant fixture");
    let expected: InvariantCoverage =
        serde_json::from_slice(&expected_json).expect("parse strict Guardian invariant fixture");
    assert_eq!(generated, expected);
    assert_eq!(json_snapshot_bytes(&generated), expected_json);

    let expected_markdown =
        std::fs::read(markdown_document_path()).expect("read Guardian invariant documentation");
    assert_eq!(markdown_document_bytes(&generated), expected_markdown);
}

#[test]
#[ignore = "set AXIAL_REGENERATE_GUARDIAN_INVARIANT_COVERAGE=1 to rewrite both artifacts"]
fn regenerate_guardian_invariant_coverage_artifacts() {
    assert_eq!(std::env::var(REGENERATE_ENV).as_deref(), Ok("1"));
    let coverage = generate_coverage();
    let json = json_snapshot_bytes(&coverage);
    let markdown = markdown_document_bytes(&coverage);
    std::fs::write(json_fixture_path(), json).expect("write Guardian invariant fixture");
    std::fs::write(markdown_document_path(), markdown)
        .expect("write Guardian invariant documentation");
}

fn generate_coverage() -> InvariantCoverage {
    assert_rule_registration();
    assert_reserved_agent_facts_are_unused();
    let kernel_cells = kernel_cells();
    assert_decision_counts(&kernel_cells);
    assert_reachable_public_copy(&kernel_cells);
    let reconciliation_hands = reconciliation_hand_coverage()
        .into_iter()
        .map(|hand| ReconciliationHandCoverage {
            admission_type: hand.admission_type.to_string(),
            rung: debug_name(&hand.rung),
            max_attempts_per_suppression_window: hand.max_attempts_per_suppression_window,
        })
        .collect::<Vec<_>>();
    assert_reconciliation_hand_coverage(&reconciliation_hands);

    InvariantCoverage {
        schema: SCHEMA.to_string(),
        invariants: vec![
            invariant("I1", "launch_failure_matrix_and_rules_registered"),
            invariant(
                "I2",
                "current_guardian_reconciliation_and_persisted_state_repair_typed_hands_registered",
            ),
            invariant("I3", "public_launch_failure_guidance_complete"),
            invariant(
                "I4",
                "current_guardian_reconciliation_and_persisted_state_repair_hand_attempt_bounds_registered",
            ),
            invariant("I5", "launch_failure_surfaces_bounded_and_redacted"),
            invariant("I6", "implemented_memory_trigger_rules_registered"),
            invariant(
                "I7",
                "typed_loader_worker_delegated_dispatch_and_named_boundary_single_assessment_complete",
            ),
            invariant(
                "I8",
                "preflight_costs_declared_reviewed_warm_cache_tier0_rotational_measurement",
            ),
            invariant("I9", "reserved_facts_unused_agent_demo_pending_phase_5"),
            invariant(
                "I10",
                "persisted_state_repair_durability_and_fail_closed_restart_contract_registered",
            ),
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
        persisted_state_startup_cells: persisted_state_startup_cells(),
        persisted_state_repair_hands: persisted_state_repair_hands(),
        rules: rule_coverage(),
        facts: fact_coverage(),
        preflight_senses: preflight_sense_coverage(),
        adapters: AdapterCoverage {
            execution: execution_adapter_coverage(),
            install: install_adapter_coverage(),
            loader_active_install: loader_active_install_adapter_coverage(),
            loader_pre_operation: loader_pre_operation_adapter_coverage(),
            readiness: readiness_adapter_coverage(),
        },
        repair_hands: repair_hand_coverage()
            .into_iter()
            .map(|(kind, diagnosis, max_attempts)| RepairHandCoverage {
                kind: kind.to_string(),
                diagnosis: diagnosis.as_str().to_string(),
                max_attempts,
            })
            .collect(),
        reconciliation_hands,
        deferred_demonstrations: vec![DeferredDemonstration {
            invariant: "I9".to_string(),
            phase: "phase_5".to_string(),
            status: "agent_fallback_execution_pending".to_string(),
        }],
    }
}

fn persisted_state_repair_hands() -> Vec<PersistedStateRepairHandCoverage> {
    let hand = persisted_state_repair_hand_coverage();
    let terminal_outcomes = hand
        .terminal_outcomes
        .iter()
        .map(debug_name)
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_outcomes,
        ["Quarantined", "Refused", "AppliedUnverified"]
    );

    vec![PersistedStateRepairHandCoverage {
        admission_type: hand.admission_type.to_string(),
        attempt_type: hand.attempt_type.to_string(),
        terminal_type: hand.terminal_type.to_string(),
        phase: debug_name(&OperationPhase::Startup),
        mode: debug_name(&GuardianMode::Managed),
        diagnosis: DiagnosisId::PersistedStateSchemaInvalid
            .as_str()
            .to_string(),
        stable_key_dimensions: hand.stable_key_dimensions.map(str::to_string).to_vec(),
        max_attempts_per_stable_key_per_suppression_window: hand
            .max_attempts_per_stable_key_per_suppression_window,
        suppression_window_hours: hand.suppression_hours,
        operation_journal_schema: hand.operation_journal_schema.to_string(),
        failure_memory_schema: hand.failure_memory_schema.to_string(),
        terminal_outcomes,
        durability_order: hand.durability_contract.map(str::to_string).to_vec(),
        restart_contracts: hand.restart_contract.map(str::to_string).to_vec(),
    }]
}

fn assert_reconciliation_hand_coverage(hands: &[ReconciliationHandCoverage]) {
    assert_eq!(hands.len(), ReconciliationRung::ALL.len());
    assert_eq!(hands.len(), 3);
    assert_eq!(
        hands
            .iter()
            .map(|hand| hand.admission_type.as_str())
            .collect::<HashSet<_>>()
            .len(),
        hands.len()
    );
    assert_eq!(
        hands
            .iter()
            .map(|hand| hand.rung.as_str())
            .collect::<HashSet<_>>()
            .len(),
        hands.len()
    );
    assert_eq!(
        hands
            .iter()
            .map(|hand| hand.rung.as_str())
            .collect::<Vec<_>>(),
        ReconciliationRung::ALL
            .iter()
            .map(debug_name)
            .collect::<Vec<_>>()
    );
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
                    .action_plan()
                    .map(|plan| plan.prerequisite.diagnosis_id)
                    .or_else(|| decision.diagnoses().first().copied())
                    .expect("total kernel decision has a diagnosis");
                cells.push(KernelCell {
                    failure_class: failure_class.as_str().to_string(),
                    phase: debug_name(&phase),
                    mode: debug_name(&mode),
                    diagnosis: diagnosis.as_str().to_string(),
                    decision: debug_name(&decision.kind()),
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

fn persisted_state_startup_cells() -> Vec<PersistedStateStartupCell> {
    let cells = persisted_state_startup_policy_cells()
        .into_iter()
        .map(|(mode, decision)| PersistedStateStartupCell {
            phase: debug_name(&OperationPhase::Startup),
            mode: debug_name(&mode),
            diagnosis: DiagnosisId::PersistedStateSchemaInvalid
                .as_str()
                .to_string(),
            candidate_actions: PERSISTED_STATE_REPAIR_CANDIDATES
                .iter()
                .map(debug_name)
                .collect(),
            decision: debug_name(&decision),
        })
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), GuardianMode::ALL.len());
    assert_eq!(
        cells
            .iter()
            .map(|cell| cell.decision.as_str())
            .collect::<Vec<_>>(),
        ["Quarantine", "AskUser", "RecordOnly"]
    );
    cells
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
                prepare.user_outcome.decision(),
                prepare.user_outcome.summary(),
                prepare.user_outcome.details(),
                prepare.user_outcome.guidance(),
            );
            assert_surface_cell(
                cells,
                failure_class,
                OperationPhase::Preparing,
                mode,
                prepare.guardian_decision.kind(),
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
                integrity_facts: &[],
                registered_artifact_repair_candidate: None,
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
                startup.user_outcome.decision(),
                startup.user_outcome.summary(),
                startup.user_outcome.details(),
                startup.user_outcome.guidance(),
            );
            assert_surface_cell(
                cells,
                failure_class,
                OperationPhase::Launching,
                mode,
                startup.guardian_decision.kind(),
            );
        }
    }
}

fn assert_public_outcome(
    decision: GuardianActionKind,
    summary: &str,
    details: &[String],
    guidance: &[String],
) {
    assert!(!summary.is_empty() && summary.len() <= 180);
    assert!(!evidence_text_looks_sensitive(summary));
    assert!(details.len() <= 6);
    assert!(guidance.len() <= 6);
    if decision != GuardianActionKind::Allow {
        assert!(!guidance.is_empty());
    }
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
        assert!(
            rule.required_conditions
                .iter()
                .chain(
                    rule.suppressions
                        .iter()
                        .flat_map(|suppression| suppression.required_conditions),
                )
                .chain(
                    rule.clauses
                        .iter()
                        .flat_map(|clause| clause.required_conditions),
                )
                .chain(
                    rule.clauses
                        .iter()
                        .flat_map(|clause| clause.evidence_fact_ids.into_iter().flatten()),
                )
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
                )
            }),
        })
        .collect()
}

fn fact_coverage() -> Vec<FactCoverage> {
    GuardianFactId::ALL
        .iter()
        .map(|fact| FactCoverage {
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

fn preflight_sense_coverage() -> Vec<PreflightSenseCoverage> {
    LaunchPreflightSenseId::ALL
        .iter()
        .map(|sense| PreflightSenseCoverage {
            id: sense.as_str().to_string(),
            declared_cost_class: sense.declared_cost_class().as_str().to_string(),
            timing_signal: LAUNCH_PREFLIGHT_SENSE_TIMING_SIGNAL.to_string(),
            measurement_status: if *sense == LaunchPreflightSenseId::IntegrityTier0 {
                "reviewed_warm_metadata_cache_rotational_measurement"
            } else {
                "pending_phase_4"
            }
            .to_string(),
            ceiling_ms: (*sense == LaunchPreflightSenseId::IntegrityTier0)
                .then_some(INTEGRITY_TIER0_CEILING_MS),
            evidence: (*sense == LaunchPreflightSenseId::IntegrityTier0).then(|| {
                "30bc856d; native Windows MSVC release; NTFS healthy SATA HDD; 512 entries; 1 warmup + 101 hot; warm metadata cache without flush; p50/p95/max 5.862/7.348/8.421 ms; cold cache not measured".to_string()
            }),
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

fn execution_adapter_coverage() -> Vec<ExecutionAdapterCell> {
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
            let diagnoses = DIAGNOSIS_RULES
                .iter()
                .filter(|rule| rule.trigger_fact_ids.contains(&fact.id))
                .map(|rule| {
                    assert!(DiagnosisId::ALL.contains(&rule.id));
                    rule.id.as_str().to_string()
                })
                .collect::<Vec<_>>();
            match kind.semantics() {
                ExecutionFactSemantics::Diagnostic => assert!(
                    !diagnoses.is_empty(),
                    "execution diagnostic has no diagnosis: {}",
                    kind.as_str()
                ),
                ExecutionFactSemantics::ConditionEvidence => assert!(
                    DIAGNOSIS_RULES
                        .iter()
                        .any(|rule| rule_references_fact(rule, &fact.id)),
                    "execution condition is not referenced by a rule: {}",
                    kind.as_str()
                ),
                ExecutionFactSemantics::NonFailure => assert!(
                    !DIAGNOSIS_RULES
                        .iter()
                        .any(|rule| rule_references_fact(rule, &fact.id)),
                    "non-failure execution source is referenced by a rule: {}",
                    kind.as_str()
                ),
            }
            ExecutionAdapterCell {
                source: kind.as_str().to_string(),
                classification: kind.semantics().as_str().to_string(),
                fact: fact_name(&fact.id),
                diagnoses,
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

fn loader_active_install_adapter_coverage() -> Vec<LoaderActiveInstallAdapterCell> {
    LoaderInstallFailureKind::ALL
        .iter()
        .map(|failure_kind| {
            let source = debug_name(failure_kind);
            let (kind, ownership, phase) = loader_install_guardian_evidence_kind(*failure_kind);
            let evidence = GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "coverage_loader_version",
                kind,
            )
            .with_ownership(ownership)
            .with_field("failure_kind", failure_kind.as_str());
            let fact = install_artifact_failure_guardian_fact(&evidence, phase);
            let assessment =
                assess_install_artifact_failure(None, GuardianMode::Managed, phase, &[evidence])
                    .expect("active loader evidence reaches a Guardian assessment");
            let outcome = assessment
                .terminal_outcome()
                .expect("active loader assessment has a terminal outcome");
            assert_public_outcome(
                outcome.user_outcome.decision(),
                outcome.user_outcome.summary(),
                outcome.user_outcome.details(),
                outcome.user_outcome.guidance(),
            );
            assert!(guardian_fact_is_registered(&fact.id));
            assert!(DiagnosisId::ALL.contains(&outcome.diagnosis_id));
            LoaderActiveInstallAdapterCell {
                source,
                phase: debug_name(&phase),
                evidence_kind: debug_name(&kind),
                fact: fact_name(&fact.id),
                diagnosis: outcome.diagnosis_id.as_str().to_string(),
                target_kind: debug_name(
                    &fact
                        .target
                        .as_ref()
                        .expect("active loader fact has a target")
                        .kind,
                ),
            }
        })
        .collect()
}

fn loader_pre_operation_adapter_coverage() -> Vec<LoaderBoundaryAdapterCell> {
    LoaderPreOperationFailureKind::ALL
        .iter()
        .map(|kind| LoaderBoundaryAdapterCell {
            source: debug_name(kind),
            boundary: "response_before_operation_allocation".to_string(),
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

fn guardian_fact_is_registered(fact: &GuardianFactId) -> bool {
    GuardianFactId::ALL.contains(fact)
}

fn fact_name(fact: &GuardianFactId) -> String {
    fact.as_str().to_string()
}

fn debug_name<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}")
}

fn json_snapshot_bytes(snapshot: &InvariantCoverage) -> Vec<u8> {
    let pretty = serde_json::to_string_pretty(snapshot).expect("serialize invariant coverage");
    format!("{pretty}\n").into_bytes()
}

fn markdown_document_bytes(snapshot: &InvariantCoverage) -> Vec<u8> {
    let mut document = format!(
        "<!-- Generated by apps/api/src/guardian/invariant_coverage.rs. Do not edit. -->\n\
         # Guardian Invariant Coverage\n\n\
         This document is a deterministic human-readable projection of Guardian's strict invariant coverage artifact. The JSON artifact remains the complete machine-readable inventory, including all kernel cells.\n\n\
         - Schema: `{}`\n\
         - Machine-readable artifact: [guardian-invariant-coverage-v4.json](../apps/api/tests/fixtures/guardian/guardian-invariant-coverage-v4.json)\n\
         - Regenerate: `AXIAL_REGENERATE_GUARDIAN_INVARIANT_COVERAGE=1 cargo test -p axial-api regenerate_guardian_invariant_coverage_artifacts -- --ignored`\n\n\
         ## Invariant Status\n",
        snapshot.schema
    );
    markdown_table(
        &mut document,
        &["Invariant", "Status"],
        snapshot
            .invariants
            .iter()
            .map(|item| vec![item.id.clone(), item.status.clone()]),
    );

    let public_cells = snapshot
        .kernel_cells
        .iter()
        .filter(|cell| cell.public_surface)
        .count();
    let adapter_sources = snapshot.adapters.execution.len()
        + snapshot.adapters.install.len()
        + snapshot.adapters.loader_active_install.len()
        + snapshot.adapters.loader_pre_operation.len()
        + snapshot.adapters.readiness.len();
    document.push_str("\n## Coverage Summary\n");
    markdown_table(
        &mut document,
        &["Surface", "Covered"],
        [
            ("Failure classes", snapshot.axes.failure_classes.len()),
            ("Operation phases", snapshot.axes.phases.len()),
            ("Guardian modes", snapshot.axes.modes.len()),
            ("Kernel cells", snapshot.kernel_cells.len()),
            (
                "Persisted-state Startup cells",
                snapshot.persisted_state_startup_cells.len(),
            ),
            (
                "Persisted-state repair hands",
                snapshot.persisted_state_repair_hands.len(),
            ),
            ("Public kernel cells", public_cells),
            ("Diagnosis rules", snapshot.rules.len()),
            ("Registered facts", snapshot.facts.len()),
            ("Preflight senses", snapshot.preflight_senses.len()),
            ("Adapter sources", adapter_sources),
            ("Repair hands", snapshot.repair_hands.len()),
            ("Reconciliation hands", snapshot.reconciliation_hands.len()),
        ]
        .map(|(surface, count)| vec![surface.to_string(), count.to_string()]),
    );

    let mut decision_counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for cell in &snapshot.kernel_cells {
        let counts = decision_counts.entry(&cell.decision).or_default();
        counts.0 += 1;
        if cell.public_surface {
            counts.1 += 1;
        }
    }
    document.push_str(
        "\n### Decision Distribution\nThe complete kernel matrix remains in the JSON artifact.\n\n",
    );
    markdown_table(
        &mut document,
        &["Decision", "All cells", "Public cells"],
        decision_counts
            .into_iter()
            .map(|(decision, (all, public))| {
                vec![decision.to_string(), all.to_string(), public.to_string()]
            }),
    );

    document.push_str("\n## Persisted-State Durable Repair Hands\n");
    markdown_table(
        &mut document,
        &[
            "Admission",
            "Attempt",
            "Terminal",
            "Phase",
            "Mode",
            "Diagnosis",
            "Stable key dimensions",
            "Maximum attempts per key/window",
            "Window (hours)",
            "Journal schema",
            "Memory schema",
            "Terminal outcomes",
        ],
        snapshot.persisted_state_repair_hands.iter().map(|hand| {
            vec![
                hand.admission_type.clone(),
                hand.attempt_type.clone(),
                hand.terminal_type.clone(),
                hand.phase.clone(),
                hand.mode.clone(),
                hand.diagnosis.clone(),
                hand.stable_key_dimensions.join(", "),
                hand.max_attempts_per_stable_key_per_suppression_window
                    .to_string(),
                hand.suppression_window_hours.to_string(),
                hand.operation_journal_schema.clone(),
                hand.failure_memory_schema.clone(),
                hand.terminal_outcomes.join(", "),
            ]
        }),
    );
    document.push_str("\n### Durability Order\n");
    markdown_table(
        &mut document,
        &["Admission", "Contract"],
        snapshot
            .persisted_state_repair_hands
            .iter()
            .flat_map(|hand| {
                hand.durability_order
                    .iter()
                    .map(|contract| vec![hand.admission_type.clone(), contract.clone()])
            }),
    );
    document.push_str("\n### Restart Contracts\n");
    markdown_table(
        &mut document,
        &["Admission", "Contract"],
        snapshot
            .persisted_state_repair_hands
            .iter()
            .flat_map(|hand| {
                hand.restart_contracts
                    .iter()
                    .map(|contract| vec![hand.admission_type.clone(), contract.clone()])
            }),
    );

    document.push_str("\n## Persisted-State Startup Policy\n");
    markdown_table(
        &mut document,
        &["Phase", "Mode", "Diagnosis", "Candidates", "Decision"],
        snapshot.persisted_state_startup_cells.iter().map(|cell| {
            vec![
                cell.phase.clone(),
                cell.mode.clone(),
                cell.diagnosis.clone(),
                cell.candidate_actions.join(", "),
                cell.decision.clone(),
            ]
        }),
    );

    document.push_str("\n## Preflight Senses\n");
    markdown_table(
        &mut document,
        &[
            "Sense",
            "Declared cost",
            "Timing signal",
            "Measurement",
            "Ceiling (ms)",
            "Evidence",
        ],
        snapshot.preflight_senses.iter().map(|sense| {
            vec![
                sense.id.clone(),
                sense.declared_cost_class.clone(),
                sense.timing_signal.clone(),
                sense.measurement_status.clone(),
                sense
                    .ceiling_ms
                    .map_or_else(|| "pending".to_string(), |value| value.to_string()),
                sense
                    .evidence
                    .clone()
                    .unwrap_or_else(|| "pending".to_string()),
            ]
        }),
    );

    document.push_str("\n## Repair Hands\n");
    markdown_table(
        &mut document,
        &["Kind", "Diagnosis", "Maximum attempts"],
        snapshot.repair_hands.iter().map(|hand| {
            vec![
                hand.kind.clone(),
                hand.diagnosis.clone(),
                hand.max_attempts.to_string(),
            ]
        }),
    );

    document.push_str("\n## Reconciliation Hands\n");
    markdown_table(
        &mut document,
        &[
            "Admission type",
            "Rung",
            "Maximum attempts per suppression window",
        ],
        snapshot.reconciliation_hands.iter().map(|hand| {
            vec![
                hand.admission_type.clone(),
                hand.rung.clone(),
                hand.max_attempts_per_suppression_window.to_string(),
            ]
        }),
    );

    document.push_str("\n## Deferred Demonstrations\n");
    markdown_table(
        &mut document,
        &["Invariant", "Phase", "Status"],
        snapshot.deferred_demonstrations.iter().map(|item| {
            vec![
                item.invariant.clone(),
                item.phase.clone(),
                item.status.clone(),
            ]
        }),
    );

    document.into_bytes()
}

fn markdown_table(
    document: &mut String,
    headers: &[&str],
    rows: impl IntoIterator<Item = Vec<String>>,
) {
    write_markdown_row(document, headers.iter().copied());
    write_markdown_row(document, headers.iter().map(|_| "---"));
    for row in rows {
        assert_eq!(row.len(), headers.len());
        write_markdown_row(document, row.iter().map(String::as_str));
    }
}

fn write_markdown_row<'a>(document: &mut String, cells: impl IntoIterator<Item = &'a str>) {
    document.push('|');
    for cell in cells {
        write!(document, " {} |", markdown_cell(cell)).expect("write Markdown row");
    }
    document.push('\n');
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
}

fn json_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guardian/guardian-invariant-coverage-v4.json")
}

fn markdown_document_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/GUARDIAN-INVARIANT-COVERAGE.md")
}
