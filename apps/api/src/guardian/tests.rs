use super::rules::{
    ActionEligibility, DIAGNOSIS_RULES, DestructiveMutationRisk, JournalRequirement,
    OwnershipRequirement, RedactionRequirement, RetryLoopSensitivity, UserIntentSensitivity,
    rule_for_diagnosis,
};
use super::{
    ActionPlanPrerequisite, Diagnosis, DiagnosisId, FactReliability, GuardianAction,
    GuardianActionKind, GuardianActionPlan, GuardianConfidence, GuardianDecision, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianSeverity, GuardianSeverity::Repairable,
    build_safety_case, diagnose_facts, guardian_fact_from_execution,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};

const GUARDIAN_DECISION_ACTIONS_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-decision-actions.json"
));
const GUARDIAN_FACT_IDS_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/guardian/guardian-fact-ids.json"
));

#[test]
fn checked_in_guardian_decision_actions_fixture_is_byte_stable() {
    let decisions =
        serde_json::from_str::<Vec<GuardianDecision>>(GUARDIAN_DECISION_ACTIONS_FIXTURE)
            .expect("decision fixture");
    let expected_kinds = [
        GuardianActionKind::Allow,
        GuardianActionKind::Warn,
        GuardianActionKind::Repair,
        GuardianActionKind::Retry,
        GuardianActionKind::Strip,
        GuardianActionKind::Downgrade,
        GuardianActionKind::Fallback,
        GuardianActionKind::Quarantine,
        GuardianActionKind::AskUser,
        GuardianActionKind::Block,
        GuardianActionKind::RecordOnly,
    ];
    assert_eq!(
        decisions
            .iter()
            .map(|decision| decision.kind)
            .collect::<Vec<_>>(),
        expected_kinds
    );
    for decision in &decisions {
        assert_fixture_action_kind(decision.kind);
        let plan = decision.action_plan.as_ref().expect("fixture action plan");
        let action = plan.actions.as_slice().first().expect("fixture action");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(
            decision.diagnoses.as_slice(),
            std::slice::from_ref(&plan.prerequisite.diagnosis_id)
        );
        assert_eq!(action.reason, plan.prerequisite.diagnosis_id);
        assert_eq!(
            plan.prerequisite.candidate_actions.as_slice(),
            &[action.kind]
        );
        assert_eq!(
            action.target.as_ref(),
            plan.prerequisite.affected_targets.first()
        );

        let decision_kind = serde_json::to_string(&decision.kind).expect("decision kind");
        let action_kind = serde_json::to_string(&action.kind).expect("action kind");
        assert_eq!(decision_kind, action_kind);
    }

    let pretty = serde_json::to_string_pretty(&decisions).expect("pretty decision fixture");
    assert_eq!(format!("{pretty}\n"), GUARDIAN_DECISION_ACTIONS_FIXTURE);

    let compact = serde_json::to_string(&decisions).expect("compact decision fixture");
    let decoded =
        serde_json::from_str::<Vec<GuardianDecision>>(&compact).expect("decode compact decisions");
    assert_eq!(
        serde_json::to_string(&decoded).expect("re-encode compact decisions"),
        compact
    );
}

fn assert_fixture_action_kind(kind: GuardianActionKind) {
    match kind {
        GuardianActionKind::Allow
        | GuardianActionKind::Warn
        | GuardianActionKind::Repair
        | GuardianActionKind::Retry
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Fallback
        | GuardianActionKind::Quarantine
        | GuardianActionKind::AskUser
        | GuardianActionKind::Block
        | GuardianActionKind::RecordOnly => {}
    }
}

#[test]
fn checked_in_guardian_fact_ids_fixture_is_byte_stable() {
    let fact_ids = serde_json::from_str::<Vec<GuardianFactId>>(GUARDIAN_FACT_IDS_FIXTURE)
        .expect("fact-id fixture");
    assert_eq!(fact_ids.as_slice(), GuardianFactId::ALL.as_slice());

    let pretty = serde_json::to_string_pretty(&fact_ids).expect("pretty fact-id fixture");
    assert_eq!(format!("{pretty}\n"), GUARDIAN_FACT_IDS_FIXTURE);

    let compact = serde_json::to_string(&fact_ids).expect("compact fact-id fixture");
    let decoded =
        serde_json::from_str::<Vec<GuardianFactId>>(&compact).expect("decode compact fact ids");
    assert_eq!(
        serde_json::to_string(&decoded).expect("re-encode compact fact ids"),
        compact
    );
    let error = serde_json::from_str::<GuardianFactId>(r#""future_fact""#)
        .expect_err("unknown fact id must be rejected")
        .to_string();
    assert!(!error.contains("future_fact"));
}

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
        id: GuardianFactId::IncompleteInstall,
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
fn managed_runtime_readiness_fact_maps_to_recoverable_diagnosis() {
    let fact = GuardianFact {
        operation_id: None,
        id: GuardianFactId::ManagedRuntimeMissing,
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
fn rule_action_eligibility_stays_internal_to_public_diagnosis_output() {
    let fact = guardian_test_fact(
        GuardianFactId::JvmArgsParseFailed,
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
fn declarative_rules_cover_exactly_the_frozen_fact_corpus() {
    let mut diagnosis_ids = std::collections::HashSet::new();
    let mut source_fact_ids = std::collections::HashSet::new();

    assert_eq!(DIAGNOSIS_RULES.len(), 46);
    for rule in DIAGNOSIS_RULES {
        assert!(diagnosis_ids.insert(rule.id), "duplicate rule {}", rule.id);
        assert!(!rule.source_fact_ids.is_empty(), "{}", rule.id);
        assert_eq!(
            rule.eligibility.redaction_requirement,
            RedactionRequirement::PublicOutcome,
            "{}",
            rule.id
        );
        for fact_id in rule.source_fact_ids {
            assert!(
                source_fact_ids.insert(*fact_id),
                "duplicate rule source {}",
                fact_id.as_str()
            );
        }
        assert_eq!(rule_for_diagnosis(rule.id), Some(rule));
    }
    assert_eq!(source_fact_ids.len(), 69);
}

#[test]
fn rule_action_eligibility_covers_each_policy_constraint_family() {
    let cases = [
        (
            DiagnosisId::JavaOverrideUnavailable,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::Classified,
                journal_requirement: JournalRequirement::RequiredForAttemptAction,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::OneAttemptOverride,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::ExplicitTechnicalIntent,
            },
        ),
        (
            DiagnosisId::ManagedRuntimeCorrupt,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_requirement: JournalRequirement::RequiredForManagedMutation,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
                destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        ),
        (
            DiagnosisId::LauncherManagedArtifactCorrupt,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::LauncherManaged,
                journal_requirement: JournalRequirement::RequiredForManagedMutation,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepairAttempt,
                destructive_mutation_risk: DestructiveMutationRisk::ManagedMutation,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        ),
        (
            DiagnosisId::DownloadUnavailable,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::Classified,
                journal_requirement: JournalRequirement::RequiredForAttemptAction,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::ProviderRetry,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        ),
        (
            DiagnosisId::ArtifactOwnershipUnsafe,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
                user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
            },
        ),
        (
            DiagnosisId::PerformanceHealthInvalid,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::CompositionManaged,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
            },
        ),
        (
            DiagnosisId::PerformanceRepeatedFailureMemory,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::CompositionManaged,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::RepeatedFailureMemory,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::PerformanceComposition,
            },
        ),
        (
            DiagnosisId::PerformanceUserOwnedConflict,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::UserOrUnknownProtected,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::UserOrUnknownProtected,
                user_intent_sensitivity: UserIntentSensitivity::UserDataBoundary,
            },
        ),
        (
            DiagnosisId::ProcessLifecycleObserved,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::None,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        ),
        (
            DiagnosisId::PersistedStateSchemaInvalid,
            ActionEligibility {
                ownership_requirement: OwnershipRequirement::None,
                journal_requirement: JournalRequirement::None,
                redaction_requirement: RedactionRequirement::PublicOutcome,
                retry_loop_sensitivity: RetryLoopSensitivity::None,
                destructive_mutation_risk: DestructiveMutationRisk::None,
                user_intent_sensitivity: UserIntentSensitivity::None,
            },
        ),
    ];

    for (diagnosis_id, expected) in cases {
        assert_eq!(
            rule_for_diagnosis(diagnosis_id)
                .expect("diagnosis rule")
                .eligibility,
            expected,
            "{diagnosis_id}"
        );
    }
}

#[test]
fn nine_multi_fact_rule_families_emit_once_with_declared_support_order() {
    let families: &[(DiagnosisId, &[GuardianFactId])] = &[
        (
            DiagnosisId::JavaOverrideUnavailable,
            &[
                GuardianFactId::JavaOverrideEmpty,
                GuardianFactId::JavaOverrideMissing,
                GuardianFactId::JavaOverrideUndefinedSentinel,
            ],
        ),
        (
            DiagnosisId::ManagedRuntimeCorrupt,
            &[
                GuardianFactId::ManagedRuntimeReadyMarkerMissing,
                GuardianFactId::ManagedRuntimeCorrupt,
            ],
        ),
        (
            DiagnosisId::JvmArgUnsupported,
            &[
                GuardianFactId::JvmArgUnsupportedGc,
                GuardianFactId::JvmArgUnlockOrderInvalid,
            ],
        ),
        (
            DiagnosisId::JvmArgUnsafeOverride,
            &[
                GuardianFactId::JvmArgReservedLauncherFlag,
                GuardianFactId::JvmArgMemoryConflict,
                GuardianFactId::JvmArgUnsafeClasspathOverride,
                GuardianFactId::JvmArgUnsafeNativePathOverride,
                GuardianFactId::JvmArgAgentOverride,
            ],
        ),
        (
            DiagnosisId::LauncherManagedArtifactCorrupt,
            &[
                GuardianFactId::ArtifactChecksumMismatch,
                GuardianFactId::ArtifactSizeMismatch,
                GuardianFactId::ManagedFileCorrupt,
                GuardianFactId::ArtifactMissing,
            ],
        ),
        (
            DiagnosisId::DownloadUnavailable,
            &[
                GuardianFactId::DownloadProviderUnavailable,
                GuardianFactId::DownloadInterrupted,
            ],
        ),
        (
            DiagnosisId::ArtifactOwnershipUnsafe,
            &[
                GuardianFactId::OwnershipUnknown,
                GuardianFactId::PrimitiveRefused,
            ],
        ),
        (
            DiagnosisId::PerformanceFallbackSelected,
            &[
                GuardianFactId::PerformanceFallbackSelected,
                GuardianFactId::PerformanceHealthFallback,
            ],
        ),
        (
            DiagnosisId::ProcessLifecycleObserved,
            &[
                GuardianFactId::ProcessSpawned,
                GuardianFactId::LauncherStopRequested,
                GuardianFactId::WatchdogKilledProcess,
                GuardianFactId::ExitCodeZero,
                GuardianFactId::ExitCodeNonzero,
                GuardianFactId::ExitCodeUnknown,
                GuardianFactId::BootMarkerObserved,
                GuardianFactId::ProcessExited,
                GuardianFactId::ProcessExitedBeforeBoot,
                GuardianFactId::ProcessExitedAfterBoot,
            ],
        ),
    ];

    for (diagnosis_id, source_fact_ids) in families {
        let facts = source_fact_ids
            .iter()
            .rev()
            .map(|fact_id| {
                guardian_test_fact(
                    *fact_id,
                    GuardianDomain::Runtime,
                    OperationPhase::Failed,
                    FactReliability::DirectStructured,
                    OwnershipClass::LauncherManaged,
                )
            })
            .collect::<Vec<_>>();
        let diagnoses = diagnose_facts(&facts, OperationPhase::Failed);
        let diagnosis = diagnoses
            .iter()
            .find(|diagnosis| diagnosis.id == *diagnosis_id)
            .unwrap_or_else(|| panic!("missing fused diagnosis {diagnosis_id}"));

        assert_eq!(
            diagnoses
                .iter()
                .filter(|diagnosis| diagnosis.id == *diagnosis_id)
                .count(),
            1
        );
        assert_eq!(diagnosis.fact_ids, *source_fact_ids);
    }
}

#[test]
fn fused_rules_fold_ownership_and_fact_values_independent_of_input_order() {
    let mut defaulted = guardian_test_fact(
        GuardianFactId::PerformanceFallbackSelected,
        GuardianDomain::Performance,
        OperationPhase::Planning,
        FactReliability::DirectStructured,
        OwnershipClass::ExternalProviderDerived,
    );
    defaulted.severity = None;
    defaulted.confidence = None;
    let mut lower = guardian_test_fact(
        GuardianFactId::PerformanceHealthFallback,
        GuardianDomain::Performance,
        OperationPhase::Planning,
        FactReliability::DirectStructured,
        OwnershipClass::UserOwned,
    );
    lower.severity = Some(GuardianSeverity::Info);
    lower.confidence = Some(GuardianConfidence::Low);

    for facts in [
        vec![defaulted.clone(), lower.clone()],
        vec![lower.clone(), defaulted.clone()],
    ] {
        let diagnosis = diagnose_facts(&facts, OperationPhase::Planning)
            .into_iter()
            .find(|diagnosis| diagnosis.id == DiagnosisId::PerformanceFallbackSelected)
            .expect("performance fallback diagnosis");
        assert_eq!(diagnosis.severity, GuardianSeverity::Warning);
        assert_eq!(diagnosis.confidence, GuardianConfidence::High);
        assert_eq!(diagnosis.ownership, OwnershipClass::UserOwned);
        assert_eq!(
            diagnosis.fact_ids,
            vec![
                GuardianFactId::PerformanceFallbackSelected,
                GuardianFactId::PerformanceHealthFallback,
            ]
        );
    }
}

#[test]
fn duplicate_source_instances_keep_distinct_real_targets_without_fake_fallbacks() {
    let mut first = guardian_test_fact(
        GuardianFactId::DownloadInterrupted,
        GuardianDomain::Download,
        OperationPhase::Downloading,
        FactReliability::DirectStructured,
        OwnershipClass::ExternalProviderDerived,
    );
    first.target = Some(target(
        "z-source",
        TargetKind::NetworkResource,
        OwnershipClass::ExternalProviderDerived,
    ));
    let mut second = first.clone();
    second.target = Some(target(
        "a-source",
        TargetKind::NetworkResource,
        OwnershipClass::ExternalProviderDerived,
    ));
    let mut without_target = first.clone();
    without_target.target = None;

    let diagnosis = diagnose_facts(
        &[first, without_target, second],
        OperationPhase::Downloading,
    )
    .remove(0);

    assert_eq!(
        diagnosis.fact_ids,
        vec![GuardianFactId::DownloadInterrupted]
    );
    assert_eq!(
        diagnosis
            .affected_targets
            .iter()
            .map(|target| target.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-source", "z-source"]
    );
}

#[test]
fn conservative_ownership_join_covers_every_pair_and_input_permutation() {
    let ownerships = [
        OwnershipClass::LauncherManaged,
        OwnershipClass::CompositionManaged,
        OwnershipClass::ExternalProviderDerived,
        OwnershipClass::UserOwned,
        OwnershipClass::Unknown,
    ];

    for (left_rank, left) in ownerships.into_iter().enumerate() {
        for (right_rank, right) in ownerships.into_iter().enumerate() {
            let expected = ownerships[left_rank.max(right_rank)];
            let left_fact = guardian_test_fact(
                GuardianFactId::DownloadInterrupted,
                GuardianDomain::Download,
                OperationPhase::Downloading,
                FactReliability::DirectStructured,
                left,
            );
            let right_fact = guardian_test_fact(
                GuardianFactId::DownloadInterrupted,
                GuardianDomain::Download,
                OperationPhase::Downloading,
                FactReliability::DirectStructured,
                right,
            );

            for facts in [
                vec![left_fact.clone(), right_fact.clone()],
                vec![right_fact.clone(), left_fact.clone()],
            ] {
                assert_eq!(
                    diagnose_facts(&facts, OperationPhase::Downloading)[0].ownership,
                    expected,
                    "{left:?} + {right:?}"
                );
            }
        }
    }
}

#[test]
fn targetless_fused_rule_emits_one_resolved_fallback() {
    let mut provider = guardian_test_fact(
        GuardianFactId::DownloadProviderUnavailable,
        GuardianDomain::Download,
        OperationPhase::Downloading,
        FactReliability::DirectStructured,
        OwnershipClass::ExternalProviderDerived,
    );
    provider.target = None;
    let mut interrupted = guardian_test_fact(
        GuardianFactId::DownloadInterrupted,
        GuardianDomain::Download,
        OperationPhase::Downloading,
        FactReliability::DirectStructured,
        OwnershipClass::UserOwned,
    );
    interrupted.target = None;

    let diagnosis = diagnose_facts(&[interrupted, provider], OperationPhase::Downloading).remove(0);

    assert_eq!(diagnosis.id, DiagnosisId::DownloadUnavailable);
    assert_eq!(diagnosis.ownership, OwnershipClass::UserOwned);
    assert_eq!(diagnosis.affected_targets.len(), 1);
    assert_eq!(
        diagnosis.affected_targets[0].kind,
        TargetKind::NetworkResource
    );
    assert_eq!(
        diagnosis.affected_targets[0].ownership,
        OwnershipClass::UserOwned
    );
    assert_eq!(
        diagnosis.affected_targets[0].id,
        "guardian-download-downloading"
    );
}

#[test]
fn diagnosis_order_follows_first_matching_input_then_rule_order() {
    let jvm = guardian_test_fact(
        GuardianFactId::JvmArgsParseFailed,
        GuardianDomain::Jvm,
        OperationPhase::Preparing,
        FactReliability::ExactClassifier,
        OwnershipClass::UserOwned,
    );
    let resource = guardian_test_fact(
        GuardianFactId::LaunchMemoryAllocationLow,
        GuardianDomain::Launch,
        OperationPhase::Preparing,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );

    for (facts, expected) in [
        (
            vec![jvm.clone(), resource.clone()],
            vec![
                DiagnosisId::JvmArgsMalformed,
                DiagnosisId::LaunchMemoryAllocationLow,
            ],
        ),
        (
            vec![resource.clone(), jvm.clone()],
            vec![
                DiagnosisId::LaunchMemoryAllocationLow,
                DiagnosisId::JvmArgsMalformed,
            ],
        ),
    ] {
        assert_eq!(
            diagnose_facts(&facts, OperationPhase::Preparing)
                .iter()
                .map(|diagnosis| diagnosis.id)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn every_rule_source_matches_outside_the_retired_phase_lists() {
    for rule in DIAGNOSIS_RULES {
        for fact_id in rule.source_fact_ids {
            let fact = guardian_test_fact(
                *fact_id,
                GuardianDomain::Unknown,
                OperationPhase::RollingBack,
                FactReliability::UserReported,
                OwnershipClass::Unknown,
            );
            assert_eq!(
                diagnose_facts(&[fact], OperationPhase::RollingBack)[0].id,
                rule.id,
                "{}",
                fact_id.as_str()
            );
        }
    }
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
            ExecutionFactKind::RuntimeUnavailableForPlatform,
            "managed_runtime_unavailable_for_platform",
        ),
        (
            ExecutionFactKind::RuntimeRosettaRequired,
            "managed_runtime_rosetta_required",
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
    let fact = guardian_test_fact(
        GuardianFactId::NoStructuredFact(OperationPhase::Launching),
        GuardianDomain::Unknown,
        OperationPhase::Launching,
        FactReliability::HeuristicClassifier,
        OwnershipClass::Unknown,
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
        id: super::DiagnosisId::InstallIncomplete,
        domain: GuardianDomain::Unknown,
        severity: GuardianSeverity::Warning,
        confidence: GuardianConfidence::Low,
        ownership: OwnershipClass::Unknown,
        phase: OperationPhase::Launching,
        fact_ids: vec![GuardianFactId::NoStructuredFact(OperationPhase::Launching)],
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
        id: super::DiagnosisId::ManagedRuntimeCorrupt,
        domain: GuardianDomain::Runtime,
        severity: GuardianSeverity::Repairable,
        confidence: GuardianConfidence::Confirmed,
        ownership: OwnershipClass::LauncherManaged,
        phase: OperationPhase::Preparing,
        fact_ids: vec![GuardianFactId::ManagedRuntimeCorrupt],
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
            reason: diagnosis.id,
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
        vec![GuardianFactId::NoStructuredFact(OperationPhase::Launching)]
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
fn safety_case_carries_diagnosis() {
    let execution_fact = ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::RuntimeWrongMajor,
        target: Some(target(
            "runtime",
            TargetKind::Runtime,
            OwnershipClass::LauncherManaged,
        )),
        fields: Vec::new(),
    };
    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Preparing);

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

fn guardian_test_fact(
    id: GuardianFactId,
    domain: GuardianDomain,
    phase: OperationPhase,
    reliability: FactReliability,
    ownership: OwnershipClass,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase,
        reliability,
        severity: None,
        confidence: None,
        ownership,
        target: Some(target(id.as_str(), TargetKind::Config, ownership)),
        fields: Vec::new(),
    }
}

fn target(id: &str, kind: TargetKind, ownership: OwnershipClass) -> TargetDescriptor {
    TargetDescriptor::new(StabilizationSystem::Guardian, kind, id, ownership)
}

fn _assert_fact_is_send_sync(_: &GuardianFact) {}
