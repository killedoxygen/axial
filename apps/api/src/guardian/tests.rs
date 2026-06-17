use super::inference_graph::{
    ActionEligibility, AffectedTargetStrategy, DestructiveMutationRisk, JournalRequirement,
    OwnershipRequirement, RedactionRequirement, RetryLoopSensitivity, UserIntentSensitivity,
    diagnosis_graph_nodes, diagnosis_node_for_fact,
};
use super::{
    ActionPlanPrerequisite, Diagnosis, FactReliability, GuardianAction, GuardianActionKind,
    GuardianActionPlan, GuardianConfidence, GuardianDomain, GuardianFact, GuardianFactId,
    GuardianMode, GuardianObservation, GuardianSeverity, GuardianSeverity::Repairable,
    build_safety_case, diagnose_facts, guardian_fact_from_execution,
    guardian_fact_from_observation,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};

#[test]
fn execution_runtime_fact_maps_to_confirmed_runtime_diagnosis() {
    let target = target(
        "runtime",
        TargetKind::Runtime,
        OwnershipClass::LauncherManaged,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::RuntimeReadyMarkerMissing,
        target: Some(target.clone()),
        fields: Vec::new(),
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
    let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    let diagnosis = &diagnoses[0];
    assert_eq!(diagnosis.id.as_str(), "managed_runtime_corrupt");
    assert_eq!(diagnosis.domain, GuardianDomain::Runtime);
    assert_eq!(diagnosis.severity, Repairable);
    assert_eq!(diagnosis.confidence, GuardianConfidence::Confirmed);
    assert_eq!(diagnosis.ownership, OwnershipClass::LauncherManaged);
    assert!(
        diagnosis
            .candidate_actions
            .contains(&GuardianActionKind::Repair)
    );
    let prerequisite = diagnosis
        .action_prerequisite()
        .expect("action prerequisite");
    assert_eq!(prerequisite.ownership, OwnershipClass::LauncherManaged);
    assert_eq!(prerequisite.confidence, GuardianConfidence::Confirmed);
}

#[test]
fn execution_java_override_sentinel_maps_to_unavailable_diagnosis() {
    let target = target(
        "instance_java_override",
        TargetKind::Config,
        OwnershipClass::UserOwned,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::RuntimeJavaOverrideUndefinedSentinel,
        target: Some(target),
        fields: vec![EvidenceField::new(
            "sentinel",
            "undefined",
            EvidenceSensitivity::Public,
        )],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
    let diagnoses = diagnose_facts(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "java_override_undefined_sentinel");
    assert_eq!(fact.domain, GuardianDomain::Runtime);
    assert_eq!(fact.reliability, FactReliability::ExactClassifier);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "java_override_unavailable");
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].ownership, OwnershipClass::UserOwned);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::Fallback)
    );
}

#[test]
fn execution_java_update_fact_maps_to_update_diagnosis() {
    let target = target(
        "manual_java",
        TargetKind::Runtime,
        OwnershipClass::UserOwned,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::RuntimeWrongUpdate,
        target: Some(target),
        fields: vec![
            EvidenceField::new("required_min_update", "312", EvidenceSensitivity::Public),
            EvidenceField::new("actual_update", "311", EvidenceSensitivity::Public),
        ],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
    let diagnoses = diagnose_facts(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "java_update_too_old");
    assert_eq!(fact.domain, GuardianDomain::Runtime);
    assert_eq!(fact.reliability, FactReliability::ValidatedProbe);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "java_runtime_update_too_old");
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::Fallback)
    );
}

#[test]
fn execution_launch_command_fact_maps_to_launch_domain() {
    let target = target(
        "session-1",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::LaunchCommandPrepared,
        target: Some(target),
        fields: vec![EvidenceField::new(
            "program",
            "launch_program",
            EvidenceSensitivity::Public,
        )],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
    let diagnoses = diagnose_facts(std::slice::from_ref(&fact), OperationPhase::Preparing);

    assert_eq!(fact.id.as_str(), "launch_command_prepared");
    assert_eq!(fact.domain, GuardianDomain::Launch);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "launch_command_prepared");
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Info);
}

#[test]
fn execution_launch_command_invalid_fact_maps_to_blocking_diagnosis() {
    let target = target(
        "session-1",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::LaunchCommandInvalid,
        target: Some(target),
        fields: vec![EvidenceField::new(
            "arg_count",
            "1",
            EvidenceSensitivity::Public,
        )],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);
    let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "launch_command_invalid");
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::Block)
    );
}

#[test]
fn launch_readiness_fact_maps_to_blocking_install_diagnosis() {
    let fact = GuardianFact {
        operation_id: None,
        id: GuardianFactId::new("incomplete_install"),
        domain: GuardianDomain::Install,
        phase: OperationPhase::Validating,
        reliability: FactReliability::DirectStructured,
        severity: Some(GuardianSeverity::Blocking),
        confidence: Some(GuardianConfidence::Confirmed),
        ownership: OwnershipClass::LauncherManaged,
        target: Some(target(
            "incomplete_install",
            TargetKind::Version,
            OwnershipClass::LauncherManaged,
        )),
        fields: Vec::new(),
    };

    let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "install_incomplete");
    assert_eq!(diagnoses[0].domain, GuardianDomain::Install);
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
    assert_eq!(
        diagnoses[0].candidate_actions,
        vec![GuardianActionKind::Block]
    );
    assert_eq!(diagnoses[0].affected_targets[0].kind, TargetKind::Version);
}

#[test]
fn persisted_state_observation_maps_to_state_warning_diagnosis() {
    let fact = guardian_fact_from_observation(
        GuardianObservation::PersistedStateSchemaInvalid,
        OperationPhase::Startup,
        Some(target(
            "persisted-state-load",
            TargetKind::Config,
            OwnershipClass::LauncherManaged,
        )),
    );

    let diagnoses = diagnose_facts(std::slice::from_ref(&fact), OperationPhase::Startup);

    assert_eq!(fact.id.as_str(), "persisted_state_schema_invalid");
    assert_eq!(fact.domain, GuardianDomain::State);
    assert_eq!(fact.reliability, FactReliability::DirectStructured);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "persisted_state_schema_invalid");
    assert_eq!(diagnoses[0].domain, GuardianDomain::State);
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Warning);
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
    assert_eq!(
        diagnoses[0].candidate_actions,
        vec![GuardianActionKind::Warn, GuardianActionKind::RecordOnly]
    );
}

#[test]
fn managed_runtime_readiness_fact_maps_to_recoverable_diagnosis() {
    let fact = GuardianFact {
        operation_id: None,
        id: GuardianFactId::new("managed_runtime_missing"),
        domain: GuardianDomain::Runtime,
        phase: OperationPhase::Validating,
        reliability: FactReliability::ExpectedMarkerAbsence,
        severity: Some(GuardianSeverity::Recoverable),
        confidence: Some(GuardianConfidence::Confirmed),
        ownership: OwnershipClass::LauncherManaged,
        target: Some(target(
            "managed_runtime",
            TargetKind::Runtime,
            OwnershipClass::LauncherManaged,
        )),
        fields: Vec::new(),
    };

    let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "managed_runtime_missing");
    assert_eq!(diagnoses[0].domain, GuardianDomain::Runtime);
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Recoverable);
    assert_eq!(
        diagnoses[0].candidate_actions,
        vec![GuardianActionKind::RecordOnly]
    );
    assert_eq!(diagnoses[0].affected_targets[0].kind, TargetKind::Runtime);
}

#[test]
fn diagnosis_inference_graph_declares_evidence_slots_and_target_strategy() {
    let fact = GuardianFact {
        operation_id: None,
        id: GuardianFactId::new("jvm_args_parse_failed"),
        domain: GuardianDomain::Jvm,
        phase: OperationPhase::Validating,
        reliability: FactReliability::ExactClassifier,
        severity: None,
        confidence: None,
        ownership: OwnershipClass::UserOwned,
        target: Some(target(
            "explicit_jvm_args",
            TargetKind::Config,
            OwnershipClass::UserOwned,
        )),
        fields: Vec::new(),
    };

    let node = diagnosis_node_for_fact(&fact).expect("graph node");

    assert_eq!(node.diagnosis_id(&fact), "jvm_args_malformed");
    assert!(diagnosis_graph_nodes().iter().any(|node| {
        node.required_facts
            .iter()
            .any(|required| required.matches_fact_id("jvm_args_parse_failed"))
    }));
    assert!(
        node.supporting_facts
            .iter()
            .any(|support| support.fact_id == "jvm_args_parse_failed" && support.weight > 0.0)
    );
    assert!(
        node.contradicting_facts
            .iter()
            .any(|fact| fact.fact_id == "boot_marker_observed")
    );
    assert!(node.phase_allowed.contains(&OperationPhase::Validating));
    assert!(node.ownership_allowed.is_empty());
    assert_eq!(
        node.target_strategy,
        AffectedTargetStrategy::FactTargetOrGuardianFallback
    );
    assert_eq!(
        node.eligibility.ownership_requirement,
        OwnershipRequirement::Classified
    );
    assert_eq!(
        node.eligibility.journal_requirement,
        JournalRequirement::RequiredForAttemptAction
    );
    assert_eq!(
        node.eligibility.redaction_requirement,
        RedactionRequirement::PublicOutcome
    );
    assert_eq!(
        node.eligibility.retry_loop_sensitivity,
        RetryLoopSensitivity::OneAttemptOverride
    );
    assert_eq!(
        node.eligibility.destructive_mutation_risk,
        DestructiveMutationRisk::None
    );
    assert_eq!(
        node.eligibility.user_intent_sensitivity,
        UserIntentSensitivity::ExplicitTechnicalIntent
    );
    assert_eq!(
        node.candidate_actions,
        &[
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block
        ]
    );
    assert_eq!(node.public_reason_template(&fact), "jvm_args_malformed");

    let evaluation = node.evaluate(
        std::slice::from_ref(&fact),
        &fact,
        OperationPhase::Validating,
    );
    assert_eq!(evaluation.action_eligibility, node.eligibility);
}

#[test]
fn graph_backed_diagnosis_truth_table_covers_current_domain_families() {
    struct Case {
        fact_id: &'static str,
        fact_domain: GuardianDomain,
        phase: OperationPhase,
        ownership: OwnershipClass,
        severity_override: Option<GuardianSeverity>,
        confidence_override: Option<GuardianConfidence>,
        expected_id: &'static str,
        expected_domain: GuardianDomain,
        expected_severity: GuardianSeverity,
        expected_confidence: GuardianConfidence,
        expected_actions: &'static [GuardianActionKind],
        expected_reason: &'static str,
    }

    let cases = [
        Case {
            fact_id: "java_override_missing",
            fact_domain: GuardianDomain::Runtime,
            phase: OperationPhase::Validating,
            ownership: OwnershipClass::UserOwned,
            severity_override: None,
            confidence_override: None,
            expected_id: "java_override_unavailable",
            expected_domain: GuardianDomain::Runtime,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[
                GuardianActionKind::Fallback,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            expected_reason: "selected_java_runtime_unavailable",
        },
        Case {
            fact_id: "managed_runtime_ready_marker_missing",
            fact_domain: GuardianDomain::Runtime,
            phase: OperationPhase::Preparing,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "managed_runtime_corrupt",
            expected_domain: GuardianDomain::Runtime,
            expected_severity: GuardianSeverity::Repairable,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Repair, GuardianActionKind::Block],
            expected_reason: "managed_runtime_needs_repair",
        },
        Case {
            fact_id: "incomplete_install",
            fact_domain: GuardianDomain::Install,
            phase: OperationPhase::Validating,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: Some(GuardianSeverity::Blocking),
            confidence_override: Some(GuardianConfidence::Confirmed),
            expected_id: "install_incomplete",
            expected_domain: GuardianDomain::Install,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Block],
            expected_reason: "incomplete_install",
        },
        Case {
            fact_id: "jvm_args_parse_failed",
            fact_domain: GuardianDomain::Jvm,
            phase: OperationPhase::Validating,
            ownership: OwnershipClass::UserOwned,
            severity_override: None,
            confidence_override: None,
            expected_id: "jvm_args_malformed",
            expected_domain: GuardianDomain::Jvm,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[
                GuardianActionKind::Strip,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            expected_reason: "jvm_args_malformed",
        },
        Case {
            fact_id: "download_provider_unavailable",
            fact_domain: GuardianDomain::Download,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::ExternalProviderDerived,
            severity_override: None,
            confidence_override: None,
            expected_id: "download_unavailable",
            expected_domain: GuardianDomain::Download,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Medium,
            expected_actions: &[
                GuardianActionKind::Retry,
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
            ],
            expected_reason: "download_unavailable",
        },
        Case {
            fact_id: "install_dependency_failed",
            fact_domain: GuardianDomain::Install,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "install_dependency_failed",
            expected_domain: GuardianDomain::Install,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Block],
            expected_reason: "install_dependency_failed",
        },
        Case {
            fact_id: "temp_file_leftover",
            fact_domain: GuardianDomain::Filesystem,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "temp_file_leftover",
            expected_domain: GuardianDomain::Filesystem,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Block],
            expected_reason: "temp_file_leftover",
        },
        Case {
            fact_id: "atomic_promotion_failed",
            fact_domain: GuardianDomain::Filesystem,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "atomic_promotion_failed",
            expected_domain: GuardianDomain::Filesystem,
            expected_severity: GuardianSeverity::Blocking,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Block],
            expected_reason: "atomic_promotion_failed",
        },
        Case {
            fact_id: "exit_code_zero",
            fact_domain: GuardianDomain::Session,
            phase: OperationPhase::Running,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "process_lifecycle_observed",
            expected_domain: GuardianDomain::Session,
            expected_severity: GuardianSeverity::Info,
            expected_confidence: GuardianConfidence::High,
            expected_actions: &[GuardianActionKind::RecordOnly],
            expected_reason: "process_lifecycle_observed",
        },
        Case {
            fact_id: "performance_health_invalid",
            fact_domain: GuardianDomain::Performance,
            phase: OperationPhase::Planning,
            ownership: OwnershipClass::CompositionManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "performance_health_invalid",
            expected_domain: GuardianDomain::Performance,
            expected_severity: GuardianSeverity::Degraded,
            expected_confidence: GuardianConfidence::High,
            expected_actions: &[GuardianActionKind::RecordOnly, GuardianActionKind::Warn],
            expected_reason: "performance_health_invalid",
        },
        Case {
            fact_id: "persisted_state_schema_invalid",
            fact_domain: GuardianDomain::State,
            phase: OperationPhase::Startup,
            ownership: OwnershipClass::LauncherManaged,
            severity_override: None,
            confidence_override: None,
            expected_id: "persisted_state_schema_invalid",
            expected_domain: GuardianDomain::State,
            expected_severity: GuardianSeverity::Warning,
            expected_confidence: GuardianConfidence::Confirmed,
            expected_actions: &[GuardianActionKind::Warn, GuardianActionKind::RecordOnly],
            expected_reason: "persisted_state_schema_invalid",
        },
    ];

    for case in cases {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::new(case.fact_id),
            domain: case.fact_domain,
            phase: case.phase,
            reliability: FactReliability::DirectStructured,
            severity: case.severity_override,
            confidence: case.confidence_override,
            ownership: case.ownership,
            target: Some(target(case.fact_id, TargetKind::Config, case.ownership)),
            fields: Vec::new(),
        };

        let diagnoses = diagnose_facts(&[fact], case.phase);

        assert_eq!(diagnoses.len(), 1, "{}", case.fact_id);
        let diagnosis = &diagnoses[0];
        assert_eq!(diagnosis.id.as_str(), case.expected_id, "{}", case.fact_id);
        assert_eq!(diagnosis.domain, case.expected_domain, "{}", case.fact_id);
        assert_eq!(
            diagnosis.severity, case.expected_severity,
            "{}",
            case.fact_id
        );
        assert_eq!(
            diagnosis.confidence, case.expected_confidence,
            "{}",
            case.fact_id
        );
        assert_eq!(diagnosis.candidate_actions, case.expected_actions);
        assert_eq!(diagnosis.public_reason_template, case.expected_reason);
        assert_eq!(diagnosis.fact_ids, vec![case.fact_id.to_string()]);
    }
}

#[test]
fn graph_evaluation_truth_table_covers_scoring_inputs_without_output_drift() {
    let provider_fact = guardian_graph_fact(
        "download_provider_unavailable",
        GuardianDomain::Download,
        OperationPhase::Downloading,
        FactReliability::DirectStructured,
        OwnershipClass::ExternalProviderDerived,
    );
    let interrupted_fact = guardian_graph_fact(
        "download_interrupted",
        GuardianDomain::Download,
        OperationPhase::Downloading,
        FactReliability::HeuristicClassifier,
        OwnershipClass::ExternalProviderDerived,
    );
    let download_node = diagnosis_node_for_fact(&provider_fact).expect("download node");

    let accumulated = download_node.evaluate(
        &[provider_fact.clone(), interrupted_fact],
        &provider_fact,
        OperationPhase::Downloading,
    );

    assert!(accumulated.required_fact_satisfied);
    assert!(accumulated.phase_compatible);
    assert_eq!(accumulated.direct_fact_count, 2);
    assert_close(accumulated.support_score, 0.888);
    assert_close(accumulated.contradiction_score, 0.0);
    assert_close(accumulated.evidence_confidence_score, 0.988);
    assert_eq!(accumulated.resolved_severity, GuardianSeverity::Blocking);
    assert_eq!(accumulated.resolved_confidence, GuardianConfidence::Medium);
    assert_eq!(
        diagnose_facts(
            std::slice::from_ref(&provider_fact),
            OperationPhase::Downloading,
        )[0]
        .confidence,
        GuardianConfidence::Medium
    );

    let jvm_fact = guardian_graph_fact(
        "jvm_args_parse_failed",
        GuardianDomain::Jvm,
        OperationPhase::Validating,
        FactReliability::ExactClassifier,
        OwnershipClass::UserOwned,
    );
    let boot_fact = guardian_graph_fact(
        "boot_marker_observed",
        GuardianDomain::Session,
        OperationPhase::Running,
        FactReliability::ProcessLifecycle,
        OwnershipClass::LauncherManaged,
    );
    let jvm_node = diagnosis_node_for_fact(&jvm_fact).expect("jvm node");
    let contradicted = jvm_node.evaluate(
        &[jvm_fact.clone(), boot_fact],
        &jvm_fact,
        OperationPhase::Validating,
    );

    assert!(contradicted.required_fact_satisfied);
    assert_close(contradicted.support_score, 0.80);
    assert_close(contradicted.contradiction_score, 0.6175);
    assert_close(contradicted.evidence_confidence_score, 0.2825);
    assert!(contradicted.selected_for_diagnosis());
    assert_eq!(
        contradicted.resolved_confidence,
        GuardianConfidence::Confirmed
    );

    let phase_matched = download_node.evaluate(
        std::slice::from_ref(&provider_fact),
        &provider_fact,
        OperationPhase::Downloading,
    );
    let phase_mismatched = download_node.evaluate(
        std::slice::from_ref(&provider_fact),
        &provider_fact,
        OperationPhase::Running,
    );

    assert!(phase_matched.phase_compatible);
    assert!(!phase_mismatched.phase_compatible);
    assert_close(phase_matched.evidence_confidence_score, 0.90);
    assert_close(phase_mismatched.evidence_confidence_score, 0.60);

    let marker_fact = guardian_graph_fact(
        "managed_runtime_missing",
        GuardianDomain::Runtime,
        OperationPhase::Validating,
        FactReliability::ExpectedMarkerAbsence,
        OwnershipClass::LauncherManaged,
    );
    let marker_node = diagnosis_node_for_fact(&marker_fact).expect("runtime marker node");
    let weak_direct = marker_node.evaluate(
        std::slice::from_ref(&marker_fact),
        &marker_fact,
        OperationPhase::Validating,
    );

    assert!(weak_direct.required_fact_satisfied);
    assert_close(weak_direct.support_score, 0.2625);
    assert_close(weak_direct.evidence_confidence_score, 0.3625);
    assert!(weak_direct.selected_for_diagnosis());
    assert_eq!(
        diagnose_facts(&[marker_fact], OperationPhase::Validating)[0]
            .id
            .as_str(),
        "managed_runtime_missing"
    );
}

#[test]
fn graph_evaluation_prioritizes_impact_scalar_and_unknown_fallback_threshold() {
    let unsafe_artifact_fact = guardian_graph_fact(
        "primitive_refused",
        GuardianDomain::Filesystem,
        OperationPhase::Installing,
        FactReliability::DirectStructured,
        OwnershipClass::Unknown,
    );
    let performance_fallback_fact = guardian_graph_fact(
        "performance_health_fallback",
        GuardianDomain::Performance,
        OperationPhase::Planning,
        FactReliability::DirectStructured,
        OwnershipClass::CompositionManaged,
    );
    let unsafe_artifact = diagnosis_node_for_fact(&unsafe_artifact_fact)
        .expect("unsafe artifact node")
        .evaluate(
            std::slice::from_ref(&unsafe_artifact_fact),
            &unsafe_artifact_fact,
            OperationPhase::Installing,
        );
    let performance_fallback = diagnosis_node_for_fact(&performance_fallback_fact)
        .expect("performance fallback node")
        .evaluate(
            std::slice::from_ref(&performance_fallback_fact),
            &performance_fallback_fact,
            OperationPhase::Planning,
        );

    assert_close(unsafe_artifact.impact_scalar, 0.95);
    assert_close(performance_fallback.impact_scalar, 0.27);
    assert!(unsafe_artifact.impact_scalar > performance_fallback.impact_scalar);
    assert_eq!(
        unsafe_artifact.resolved_severity,
        GuardianSeverity::Blocking
    );
    assert_eq!(
        performance_fallback.resolved_severity,
        GuardianSeverity::Warning
    );

    let unknown_fact = guardian_fact_from_observation(
        GuardianObservation::Unknown("provider_payload_changed".to_string()),
        OperationPhase::Downloading,
        Some(target(
            "download-provider",
            TargetKind::NetworkResource,
            OwnershipClass::ExternalProviderDerived,
        )),
    );
    let download_node = diagnosis_graph_nodes()
        .iter()
        .find(|node| {
            node.required_facts.iter().any(|required| {
                required.matches_fact_id("download_provider_unavailable")
                    || required.matches_fact_id("download_interrupted")
            })
        })
        .expect("download graph node");
    let below_threshold = download_node.evaluate(
        std::slice::from_ref(&unknown_fact),
        &unknown_fact,
        OperationPhase::Downloading,
    );

    assert!(!below_threshold.required_fact_satisfied);
    assert_close(below_threshold.support_score, 0.0);
    assert_close(below_threshold.evidence_confidence_score, 0.0);
    assert!(!below_threshold.selected_for_diagnosis());

    let diagnoses = diagnose_facts(&[unknown_fact], OperationPhase::Downloading);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "unknown_failure_downloading");
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Low);
}

#[test]
fn graph_action_eligibility_truth_table_covers_hard_constraint_inputs() {
    struct Case {
        fact_id: &'static str,
        domain: GuardianDomain,
        phase: OperationPhase,
        ownership: OwnershipClass,
        expected: ActionEligibility,
    }

    let cases = [
        Case {
            fact_id: "java_override_missing",
            domain: GuardianDomain::Runtime,
            phase: OperationPhase::Validating,
            ownership: OwnershipClass::UserOwned,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::Classified,
                journal_requirement: JournalRequirement::RequiredForAttemptAction,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::OneAttemptOverride,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
            },
        },
        Case {
            fact_id: "managed_runtime_ready_marker_missing",
            domain: GuardianDomain::Runtime,
            phase: OperationPhase::Preparing,
            ownership: OwnershipClass::LauncherManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_requirement: JournalRequirement::RequiredForManagedMutation,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
                destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        },
        Case {
            fact_id: "artifact_checksum_mismatch",
            domain: GuardianDomain::Install,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::LauncherManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_requirement: JournalRequirement::RequiredForManagedMutation,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
                destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        },
        Case {
            fact_id: "download_provider_unavailable",
            domain: GuardianDomain::Download,
            phase: OperationPhase::Downloading,
            ownership: OwnershipClass::ExternalProviderDerived,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::Classified,
                journal_requirement: JournalRequirement::RequiredForAttemptAction,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::ProviderRetry,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        },
        Case {
            fact_id: "primitive_refused",
            domain: GuardianDomain::Filesystem,
            phase: OperationPhase::Installing,
            ownership: OwnershipClass::Unknown,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
                user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
            },
        },
        Case {
            fact_id: "performance_health_invalid",
            domain: GuardianDomain::Performance,
            phase: OperationPhase::Planning,
            ownership: OwnershipClass::CompositionManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::CompositionManaged,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
            },
        },
        Case {
            fact_id: "performance_repeated_failure_memory",
            domain: GuardianDomain::Performance,
            phase: OperationPhase::Planning,
            ownership: OwnershipClass::CompositionManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::CompositionManaged,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepeatedFailureMemory,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
            },
        },
        Case {
            fact_id: "performance_user_owned_conflict",
            domain: GuardianDomain::Performance,
            phase: OperationPhase::Planning,
            ownership: OwnershipClass::UserOwned,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
                user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
            },
        },
        Case {
            fact_id: "exit_code_zero",
            domain: GuardianDomain::Session,
            phase: OperationPhase::Running,
            ownership: OwnershipClass::LauncherManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::None,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        },
        Case {
            fact_id: "persisted_state_schema_invalid",
            domain: GuardianDomain::State,
            phase: OperationPhase::Startup,
            ownership: OwnershipClass::LauncherManaged,
            expected: ActionEligibility {
                ownership_requirement: OwnershipRequirement::None,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        },
    ];

    for case in cases {
        let fact = guardian_graph_fact(
            case.fact_id,
            case.domain,
            case.phase,
            FactReliability::DirectStructured,
            case.ownership,
        );
        let node = diagnosis_node_for_fact(&fact).expect(case.fact_id);
        let evaluation = node.evaluate(std::slice::from_ref(&fact), &fact, case.phase);

        assert_eq!(node.eligibility, case.expected, "{}", case.fact_id);
        assert_eq!(
            evaluation.action_eligibility, case.expected,
            "{}",
            case.fact_id
        );
    }

    assert!(diagnosis_graph_nodes().iter().all(|node| {
        node.eligibility.redaction_requirement == RedactionRequirement::PublicOutcome
    }));
}

#[test]
fn graph_action_eligibility_stays_internal_to_public_diagnosis_output() {
    let fact = guardian_graph_fact(
        "jvm_args_parse_failed",
        GuardianDomain::Jvm,
        OperationPhase::Validating,
        FactReliability::ExactClassifier,
        OwnershipClass::UserOwned,
    );

    let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);
    let encoded = serde_json::to_string(&diagnoses[0]).expect("diagnosis json");

    assert!(!encoded.contains("action_eligibility"));
    assert!(!encoded.contains("journal_requirement"));
    assert!(!encoded.contains("destructive_mutation_risk"));
    assert_eq!(diagnoses[0].id.as_str(), "jvm_args_malformed");
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
    assert_eq!(
        diagnoses[0].candidate_actions,
        vec![
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block
        ]
    );
}

#[test]
fn execution_jvm_parse_fact_maps_to_malformed_diagnosis() {
    let target = target(
        "explicit_jvm_args",
        TargetKind::Config,
        OwnershipClass::UserOwned,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::JvmArgsParseFailed,
        target: Some(target),
        fields: vec![EvidenceField::new(
            "raw",
            r#""unterminated -Xmx8G C:\Users\Alice"#,
            EvidenceSensitivity::Internal,
        )],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
    let diagnoses = diagnose_facts(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "jvm_args_parse_failed");
    assert_eq!(fact.domain, GuardianDomain::Jvm);
    assert_eq!(fact.reliability, FactReliability::ExactClassifier);
    assert!(fact.fields.is_empty());
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "jvm_args_malformed");
    assert_eq!(diagnoses[0].severity, GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Confirmed);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::Strip)
    );
}

#[test]
fn execution_jvm_unsafe_fact_maps_to_unsafe_override_diagnosis() {
    let target = target(
        "explicit_jvm_args",
        TargetKind::Config,
        OwnershipClass::UserOwned,
    );
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::JvmArgAgentOverride,
        target: Some(target),
        fields: vec![EvidenceField::new(
            "arg_family",
            "agent",
            EvidenceSensitivity::Public,
        )],
    };

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
    let diagnoses = diagnose_facts(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "jvm_arg_unsafe_override");
    assert_eq!(diagnoses[0].domain, GuardianDomain::Jvm);
    assert_eq!(diagnoses[0].ownership, OwnershipClass::UserOwned);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::AskUser)
    );
}

#[test]
fn execution_download_and_process_facts_map_to_guardian_fact_ids() {
    let target = target(
        "session",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    );
    let cases = [
        (
            ExecutionFactKind::DownloadProviderFailure,
            "download_provider_unavailable",
        ),
        (
            ExecutionFactKind::DownloadInterrupted,
            "download_interrupted",
        ),
        (
            ExecutionFactKind::DownloadChecksumMismatch,
            "artifact_checksum_mismatch",
        ),
        (
            ExecutionFactKind::DownloadSizeMismatch,
            "artifact_size_mismatch",
        ),
        (
            ExecutionFactKind::DownloadTempDiscarded,
            "download_temp_discarded",
        ),
        (
            ExecutionFactKind::DownloadTempWriteFailed,
            "temp_file_leftover",
        ),
        (
            ExecutionFactKind::DownloadPromotionFailed,
            "atomic_promotion_failed",
        ),
        (
            ExecutionFactKind::DownloadPromoted,
            "atomic_promotion_completed",
        ),
        (
            ExecutionFactKind::InstallDependencyFailed,
            "install_dependency_failed",
        ),
        (
            ExecutionFactKind::ProcessStopIntent,
            "launcher_stop_requested",
        ),
        (
            ExecutionFactKind::ProcessWatchdogAction,
            "watchdog_killed_process",
        ),
        (
            ExecutionFactKind::ProcessBootEvidence,
            "boot_marker_observed",
        ),
    ];

    for (kind, expected) in cases {
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind,
                target: Some(target.clone()),
                fields: Vec::new(),
            },
            OperationPhase::Running,
        );
        assert_eq!(fact.id.as_str(), expected);
    }
}

#[test]
fn exit_code_fact_maps_zero_and_nonzero_without_exit_classification() {
    let target = target(
        "session",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    );
    for (exit_code, expected) in [(0, "exit_code_zero"), (1, "exit_code_nonzero")] {
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind: ExecutionFactKind::ProcessExitCode,
                target: Some(target.clone()),
                fields: vec![EvidenceField::new(
                    "exit_code",
                    exit_code.to_string(),
                    EvidenceSensitivity::Public,
                )],
            },
            OperationPhase::Running,
        );
        assert_eq!(fact.id.as_str(), expected);
        let diagnoses = diagnose_facts(&[fact], OperationPhase::Running);
        assert_eq!(diagnoses[0].id.as_str(), "process_lifecycle_observed");
        assert_eq!(
            diagnoses[0].candidate_actions,
            vec![GuardianActionKind::RecordOnly]
        );
    }
}

#[test]
fn unknown_facts_produce_low_confidence_unknown_diagnosis() {
    let fact = guardian_fact_from_observation(
        GuardianObservation::Unknown("unexpected_signal".to_string()),
        OperationPhase::Launching,
        Some(target(
            "unknown",
            TargetKind::Session,
            OwnershipClass::Unknown,
        )),
    );

    let diagnoses = diagnose_facts(&[fact], OperationPhase::Launching);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "unknown_failure_launching");
    assert_eq!(diagnoses[0].domain, GuardianDomain::Unknown);
    assert_eq!(diagnoses[0].confidence, GuardianConfidence::Low);
    assert!(
        diagnoses[0]
            .candidate_actions
            .contains(&GuardianActionKind::RecordOnly)
    );
}

#[test]
fn action_prerequisite_requires_target_and_candidate_action() {
    let mut diagnosis = Diagnosis {
        id: super::DiagnosisId::new("incomplete"),
        domain: GuardianDomain::Unknown,
        severity: GuardianSeverity::Warning,
        confidence: GuardianConfidence::Low,
        ownership: OwnershipClass::Unknown,
        phase: OperationPhase::Launching,
        fact_ids: vec!["fact".to_string()],
        affected_targets: Vec::new(),
        impact: Default::default(),
        candidate_actions: vec![GuardianActionKind::RecordOnly],
        public_reason_template: "unknown".to_string(),
    };
    assert!(diagnosis.action_prerequisite().is_err());

    diagnosis.affected_targets.push(target(
        "target",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    ));
    diagnosis.candidate_actions.clear();
    assert!(diagnosis.action_prerequisite().is_err());

    diagnosis
        .candidate_actions
        .push(GuardianActionKind::RecordOnly);
    let prerequisite: ActionPlanPrerequisite = diagnosis
        .action_prerequisite()
        .expect("complete prerequisite");
    assert_eq!(prerequisite.confidence, GuardianConfidence::Low);
    assert_eq!(prerequisite.ownership, OwnershipClass::Unknown);
}

#[test]
fn action_plan_representation_carries_prerequisite_metadata() {
    let target = target(
        "runtime",
        TargetKind::Runtime,
        OwnershipClass::LauncherManaged,
    );
    let diagnosis = Diagnosis {
        id: super::DiagnosisId::new("managed_runtime_corrupt"),
        domain: GuardianDomain::Runtime,
        severity: GuardianSeverity::Repairable,
        confidence: GuardianConfidence::Confirmed,
        ownership: OwnershipClass::LauncherManaged,
        phase: OperationPhase::Preparing,
        fact_ids: vec!["managed_runtime_corrupt".to_string()],
        affected_targets: vec![target.clone()],
        impact: Default::default(),
        candidate_actions: vec![GuardianActionKind::Repair],
        public_reason_template: "managed_runtime_needs_repair".to_string(),
    };
    let prerequisite = diagnosis
        .action_prerequisite()
        .expect("complete prerequisite");
    let plan = GuardianActionPlan::new(
        StabilizationSystem::Guardian,
        prerequisite,
        vec![GuardianAction {
            kind: GuardianActionKind::Repair,
            target: Some(target),
            reason: diagnosis.id.clone(),
        }],
    );

    assert_eq!(plan.prerequisite.confidence, GuardianConfidence::Confirmed);
    assert_eq!(plan.prerequisite.ownership, OwnershipClass::LauncherManaged);
    let encoded = serde_json::to_string(&plan).expect("plan json");
    assert!(encoded.contains("prerequisite"));
    assert!(encoded.contains("Confirmed"));
    assert!(encoded.contains("LauncherManaged"));
}

#[test]
fn targetless_fact_receives_guardian_fallback_target() {
    let fact = guardian_fact_from_execution(
        &ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::RuntimeProbeFailed,
            target: None,
            fields: Vec::new(),
        },
        OperationPhase::Preparing,
    );

    let diagnoses = diagnose_facts(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "java_probe_failed");
    assert_eq!(
        diagnoses[0].affected_targets[0],
        TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Runtime,
            "guardian-runtime-preparing",
            OwnershipClass::Unknown,
        )
    );
    diagnoses[0]
        .action_prerequisite()
        .expect("fallback target makes prerequisite representable");
}

#[test]
fn empty_fact_set_unknown_diagnosis_has_fallback_target() {
    let diagnoses = diagnose_facts(&[], OperationPhase::Launching);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id.as_str(), "unknown_failure_launching");
    assert_eq!(
        diagnoses[0].fact_ids,
        vec!["no_structured_fact_launching".to_string()]
    );
    assert_eq!(
        diagnoses[0].affected_targets[0],
        TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Config,
            "guardian-unknown-launching",
            OwnershipClass::Unknown,
        )
    );
}

#[test]
fn guardian_fact_redaction_drops_raw_paths_jvm_args_and_tokens() {
    let target = TargetDescriptor {
        system: StabilizationSystem::Execution,
        kind: TargetKind::Runtime,
        id: r"C:\Users\Alice\java.exe --accessToken abc".to_string(),
        ownership: OwnershipClass::UserOwned,
    };
    let fact = guardian_fact_from_execution(
        &ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::RuntimeProbeFailed,
            target: Some(target),
            fields: vec![
                EvidenceField::new(
                    "raw",
                    "/home/alice/.jdks/java -Xmx8192M --accessToken secret",
                    EvidenceSensitivity::Public,
                ),
                EvidenceField::new("safe", "probe_failed", EvidenceSensitivity::Public),
            ],
        },
        OperationPhase::Preparing,
    );

    let encoded = serde_json::to_string(&fact).expect("fact json");
    let lower = encoded.to_ascii_lowercase();
    assert!(lower.contains("probe_failed"));
    assert!(!lower.contains("/home/"));
    assert!(!lower.contains("users\\\\alice"));
    assert!(!lower.contains("java.exe"));
    assert!(!lower.contains("-xmx"));
    assert!(!lower.contains("--accesstoken"));
    assert!(!lower.contains("secret"));
}

#[test]
fn safety_case_carries_diagnosis_and_hard_constraints() {
    let fact = guardian_fact_from_observation(
        GuardianObservation::JavaMajorMismatch,
        OperationPhase::Preparing,
        Some(target(
            "runtime",
            TargetKind::Runtime,
            OwnershipClass::LauncherManaged,
        )),
    );

    let safety_case = build_safety_case(
        None,
        GuardianMode::Managed,
        OperationPhase::Preparing,
        &[fact],
    );

    assert_eq!(safety_case.diagnoses.len(), 1);
    assert_eq!(
        safety_case.diagnoses[0].id.as_str(),
        "java_runtime_major_mismatch"
    );
    assert!(!safety_case.hard_constraints.is_empty());
}

#[test]
fn impact_vector_uses_priority_weighting() {
    let vector = super::GuardianImpactVector {
        privacy_risk: 0.0,
        data_loss_risk: 0.0,
        launchability_impact: 0.8,
        state_corruption_impact: 0.4,
        user_intent_impact: 0.2,
        performance_impact: 1.0,
        host_stability_impact: 0.3,
    };

    assert!((vector.scalar_severity() - 0.72).abs() < f32::EPSILON);
}

fn guardian_graph_fact(
    id: &str,
    domain: GuardianDomain,
    phase: OperationPhase,
    reliability: FactReliability,
    ownership: OwnershipClass,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id: GuardianFactId::new(id),
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target: Some(target(id, TargetKind::Config, ownership)),
        fields: Vec::new(),
    }
}

fn assert_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 0.0001,
        "expected {expected}, got {actual}"
    );
}

fn target(id: &str, kind: TargetKind, ownership: OwnershipClass) -> TargetDescriptor {
    TargetDescriptor::new(StabilizationSystem::Guardian, kind, id, ownership)
}

fn _assert_fact_is_send_sync(_: &GuardianFact) {}
