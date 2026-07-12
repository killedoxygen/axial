use super::rules::{DIAGNOSIS_RULES, rule_for_diagnosis};
use super::{
    DiagnosisId, FactReliability, GuardianAction, GuardianActionKind, GuardianActionPlan,
    GuardianConfidence, GuardianCopyRequest, GuardianDecision, GuardianDomain, GuardianFact,
    GuardianFactId, GuardianMode, GuardianSeverity, GuardianSeverity::Repairable,
    author_guardian_copy, build_safety_case, decide_guardian_policy, diagnose,
    guardian_fact_from_execution,
};
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct GuardianDecisionFixture {
    operation_id: Option<OperationId>,
    mode: GuardianMode,
    kind: GuardianActionKind,
    diagnoses: Vec<DiagnosisId>,
    action_plan: Option<GuardianActionPlan>,
}

impl GuardianDecisionFixture {
    fn into_decision(self) -> GuardianDecision {
        GuardianDecision::for_test(
            self.operation_id,
            self.mode,
            self.kind,
            self.diagnoses,
            self.action_plan,
        )
    }
}

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
        serde_json::from_str::<Vec<GuardianDecisionFixture>>(GUARDIAN_DECISION_ACTIONS_FIXTURE)
            .expect("decision fixture")
            .into_iter()
            .map(GuardianDecisionFixture::into_decision)
            .collect::<Vec<_>>();
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
            .map(GuardianDecision::kind)
            .collect::<Vec<_>>(),
        expected_kinds
    );
    for decision in &decisions {
        assert_fixture_action_kind(decision.kind());
        let plan = decision.action_plan().expect("fixture action plan");
        let action = plan.actions.as_slice().first().expect("fixture action");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(
            decision.diagnoses(),
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

        let decision_kind = serde_json::to_string(&decision.kind()).expect("decision kind");
        let action_kind = serde_json::to_string(&action.kind).expect("action kind");
        assert_eq!(decision_kind, action_kind);
    }

    let pretty = serde_json::to_string_pretty(&decisions).expect("pretty decision fixture");
    assert_eq!(format!("{pretty}\n"), GUARDIAN_DECISION_ACTIONS_FIXTURE);

    let compact = serde_json::to_string(&decisions).expect("compact decision fixture");
    let decoded = serde_json::from_str::<Vec<GuardianDecisionFixture>>(&compact)
        .expect("decode compact decisions");
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
    let diagnoses = diagnose(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    let diagnosis = &diagnoses[0];
    assert_eq!(diagnosis.id().as_str(), "managed_runtime_corrupt");
    assert_eq!(diagnosis.domain(), GuardianDomain::Runtime);
    assert_eq!(diagnosis.severity(), Repairable);
    assert_eq!(diagnosis.confidence(), GuardianConfidence::Confirmed);
    assert_eq!(diagnosis.ownership(), OwnershipClass::LauncherManaged);
    assert!(
        diagnosis
            .candidate_actions()
            .contains(&GuardianActionKind::Repair)
    );
    let prerequisite = diagnosis.action_prerequisite();
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
    let diagnoses = diagnose(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "java_override_undefined_sentinel");
    assert_eq!(fact.domain, GuardianDomain::Runtime);
    assert_eq!(fact.reliability, FactReliability::ExactClassifier);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "java_override_unavailable");
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].ownership(), OwnershipClass::UserOwned);
    assert!(
        diagnoses[0]
            .candidate_actions()
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
    let diagnoses = diagnose(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "java_update_too_old");
    assert_eq!(fact.domain, GuardianDomain::Runtime);
    assert_eq!(fact.reliability, FactReliability::ValidatedProbe);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "java_runtime_update_too_old");
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Blocking);
    assert!(
        diagnoses[0]
            .candidate_actions()
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
    let diagnoses = diagnose(std::slice::from_ref(&fact), OperationPhase::Preparing);

    assert_eq!(fact.id.as_str(), "launch_command_prepared");
    assert_eq!(fact.domain, GuardianDomain::Launch);
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "launch_command_prepared");
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Info);
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
    let diagnoses = diagnose(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "launch_command_invalid");
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Blocking);
    assert!(
        diagnoses[0]
            .candidate_actions()
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

    let diagnoses = diagnose(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "install_incomplete");
    assert_eq!(diagnoses[0].domain(), GuardianDomain::Install);
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].confidence(), GuardianConfidence::Confirmed);
    assert_eq!(
        diagnoses[0].candidate_actions(),
        vec![GuardianActionKind::Block]
    );
    assert_eq!(diagnoses[0].affected_targets()[0].kind, TargetKind::Version);
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

    let diagnoses = diagnose(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "managed_runtime_missing");
    assert_eq!(diagnoses[0].domain(), GuardianDomain::Runtime);
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Recoverable);
    assert_eq!(
        diagnoses[0].candidate_actions(),
        vec![GuardianActionKind::RecordOnly]
    );
    assert_eq!(diagnoses[0].affected_targets()[0].kind, TargetKind::Runtime);
}

#[test]
fn declarative_rules_have_unique_ids_and_keep_conditions_out_of_evidence() {
    let mut diagnosis_ids = std::collections::HashSet::new();
    let conditions = [
        GuardianFactId::LaunchFailureClassified,
        GuardianFactId::LaunchRuntimeFallbackAvailable,
        GuardianFactId::LaunchJvmStripAvailable,
        GuardianFactId::LaunchJvmPresetDowngradeAvailable,
    ];

    assert_eq!(DIAGNOSIS_RULES.len(), 60);
    for rule in DIAGNOSIS_RULES {
        assert!(diagnosis_ids.insert(rule.id), "duplicate rule {}", rule.id);
        assert!(!rule.trigger_fact_ids.is_empty(), "{}", rule.id);
        assert!(!rule.evidence_fact_ids.is_empty(), "{}", rule.id);
        for condition in conditions {
            assert!(!rule.trigger_fact_ids.contains(&condition), "{}", rule.id);
            assert!(!rule.evidence_fact_ids.contains(&condition), "{}", rule.id);
            assert!(
                rule.clauses.iter().all(|clause| clause
                    .evidence_fact_ids
                    .is_none_or(|evidence| !evidence.contains(&condition))),
                "{}",
                rule.id
            );
        }
        assert_eq!(rule_for_diagnosis(rule.id), Some(rule));
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
                GuardianFactId::ProcessKilled,
                GuardianFactId::WatchdogActionObserved,
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

    for (diagnosis_id, expected_fact_ids) in families {
        let facts = expected_fact_ids
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
        let diagnoses = diagnose(&facts, OperationPhase::Failed);
        let diagnosis = diagnoses
            .iter()
            .find(|diagnosis| diagnosis.id() == *diagnosis_id)
            .unwrap_or_else(|| panic!("missing fused diagnosis {diagnosis_id}"));

        assert_eq!(
            diagnoses
                .iter()
                .filter(|diagnosis| diagnosis.id() == *diagnosis_id)
                .count(),
            1
        );
        assert_eq!(diagnosis.fact_ids(), *expected_fact_ids);
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
        let diagnosis = diagnose(&facts, OperationPhase::Planning)
            .into_iter()
            .find(|diagnosis| diagnosis.id() == DiagnosisId::PerformanceFallbackSelected)
            .expect("performance fallback diagnosis");
        assert_eq!(diagnosis.severity(), GuardianSeverity::Warning);
        assert_eq!(diagnosis.confidence(), GuardianConfidence::High);
        assert_eq!(diagnosis.ownership(), OwnershipClass::UserOwned);
        assert_eq!(
            diagnosis.fact_ids(),
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

    let diagnosis = diagnose(
        &[first, without_target, second],
        OperationPhase::Downloading,
    )
    .remove(0);

    assert_eq!(
        diagnosis.fact_ids(),
        vec![GuardianFactId::DownloadInterrupted]
    );
    assert_eq!(
        diagnosis
            .affected_targets()
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
                    diagnose(&facts, OperationPhase::Downloading)[0].ownership(),
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

    let diagnosis = diagnose(&[interrupted, provider], OperationPhase::Downloading).remove(0);

    assert_eq!(diagnosis.id(), DiagnosisId::DownloadUnavailable);
    assert_eq!(diagnosis.ownership(), OwnershipClass::UserOwned);
    assert_eq!(diagnosis.affected_targets().len(), 1);
    assert_eq!(
        diagnosis.affected_targets()[0].kind,
        TargetKind::NetworkResource
    );
    assert_eq!(
        diagnosis.affected_targets()[0].ownership,
        OwnershipClass::UserOwned
    );
    assert_eq!(
        diagnosis.affected_targets()[0].id,
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
            diagnose(&facts, OperationPhase::Preparing)
                .iter()
                .map(|diagnosis| diagnosis.id())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn phase_agnostic_rule_triggers_match_in_rollback() {
    for rule in DIAGNOSIS_RULES {
        if !rule.active_phases.is_empty() || !rule.required_conditions.is_empty() {
            continue;
        }
        for fact_id in rule.trigger_fact_ids {
            let fact = guardian_test_fact(
                *fact_id,
                GuardianDomain::Unknown,
                OperationPhase::RollingBack,
                FactReliability::UserReported,
                OwnershipClass::Unknown,
            );
            assert!(
                diagnose(&[fact], OperationPhase::RollingBack)
                    .iter()
                    .any(|diagnosis| diagnosis.id() == rule.id),
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
    let diagnoses = diagnose(std::slice::from_ref(&fact), OperationPhase::Validating);

    assert_eq!(fact.id.as_str(), "jvm_args_parse_failed");
    assert_eq!(fact.domain, GuardianDomain::Jvm);
    assert_eq!(fact.reliability, FactReliability::ExactClassifier);
    assert!(fact.fields.is_empty());
    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "jvm_args_malformed");
    assert_eq!(diagnoses[0].severity(), GuardianSeverity::Blocking);
    assert_eq!(diagnoses[0].confidence(), GuardianConfidence::Confirmed);
    assert!(
        diagnoses[0]
            .candidate_actions()
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
    let diagnoses = diagnose(&[fact], OperationPhase::Validating);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "jvm_arg_unsafe_override");
    assert_eq!(diagnoses[0].domain(), GuardianDomain::Jvm);
    assert_eq!(diagnoses[0].ownership(), OwnershipClass::UserOwned);
    assert!(
        diagnoses[0]
            .candidate_actions()
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
            "temp_file_write_failed",
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
            ExecutionFactKind::InstallExecutionFailed,
            "install_execution_failed",
        ),
        (
            ExecutionFactKind::InstallProcessorFailed,
            "install_processor_failed",
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
            "watchdog_action_observed",
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
        let diagnoses = diagnose(&[fact], OperationPhase::Running);
        assert_eq!(diagnoses[0].id().as_str(), "process_lifecycle_observed");
        assert_eq!(
            diagnoses[0].candidate_actions(),
            vec![GuardianActionKind::RecordOnly]
        );
    }

    let unknown = guardian_fact_from_execution(
        &ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::ProcessExitCode,
            target: Some(target),
            fields: Vec::new(),
        },
        OperationPhase::Running,
    );
    assert_eq!(unknown.id, GuardianFactId::ExitCodeUnknown);
}

#[test]
fn process_kill_and_watchdog_fields_preserve_exact_lifecycle_meaning() {
    let target = target(
        "session",
        TargetKind::Session,
        OwnershipClass::LauncherManaged,
    );
    let cases = [
        (
            ExecutionFactKind::ProcessKilled,
            Some(("reason", "startup_watchdog")),
            GuardianFactId::WatchdogKilledProcess,
        ),
        (
            ExecutionFactKind::ProcessKilled,
            Some(("reason", "user_requested")),
            GuardianFactId::ProcessKilled,
        ),
        (
            ExecutionFactKind::ProcessKilled,
            Some(("reason", "launcher_shutdown")),
            GuardianFactId::ProcessKilled,
        ),
        (
            ExecutionFactKind::ProcessKilled,
            Some(("reason", "unknown")),
            GuardianFactId::ProcessKilled,
        ),
        (
            ExecutionFactKind::ProcessKilled,
            None,
            GuardianFactId::ProcessKilled,
        ),
        (
            ExecutionFactKind::ProcessWatchdogAction,
            Some(("action", "startup_no_output_kill")),
            GuardianFactId::WatchdogKilledProcess,
        ),
        (
            ExecutionFactKind::ProcessWatchdogAction,
            Some(("action", "startup_window_expired")),
            GuardianFactId::StartupWindowExpired,
        ),
        (
            ExecutionFactKind::ProcessWatchdogAction,
            Some(("action", "unknown")),
            GuardianFactId::WatchdogActionObserved,
        ),
        (
            ExecutionFactKind::ProcessWatchdogAction,
            None,
            GuardianFactId::WatchdogActionObserved,
        ),
    ];

    for (kind, field, expected) in cases {
        let fields = field
            .map(|(key, value)| vec![EvidenceField::new(key, value, EvidenceSensitivity::Public)])
            .unwrap_or_default();
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind,
                target: Some(target.clone()),
                fields,
            },
            OperationPhase::Running,
        );
        assert_eq!(fact.id, expected);
    }
}

#[test]
fn runtime_missing_executable_preserves_managed_and_user_owned_branches() {
    for (ownership, expected) in [
        (
            OwnershipClass::LauncherManaged,
            GuardianFactId::ManagedRuntimeMissing,
        ),
        (
            OwnershipClass::UserOwned,
            GuardianFactId::JavaOverrideMissing,
        ),
    ] {
        let fact = guardian_fact_from_execution(
            &ExecutionFact {
                operation_id: None,
                kind: ExecutionFactKind::RuntimeMissingExecutable,
                target: Some(target("runtime", TargetKind::Runtime, ownership)),
                fields: Vec::new(),
            },
            OperationPhase::Validating,
        );
        assert_eq!(fact.id, expected);
    }
}

#[test]
fn unclassified_exit_context_stays_out_of_shared_rule_evidence() {
    let cases = [
        (
            GuardianFactId::JavaMajorMismatch,
            GuardianDomain::Runtime,
            DiagnosisId::JavaRuntimeMajorMismatch,
        ),
        (
            GuardianFactId::JvmArgUnsupportedGc,
            GuardianDomain::Jvm,
            DiagnosisId::JvmArgUnsupported,
        ),
        (
            GuardianFactId::LauncherManagedArtifactSignatureCorruption,
            GuardianDomain::Download,
            DiagnosisId::LauncherManagedArtifactSignatureCorrupt,
        ),
    ];

    for (cause_id, domain, expected_id) in cases {
        let process = guardian_test_fact(
            GuardianFactId::ProcessExitedBeforeBoot,
            GuardianDomain::Session,
            OperationPhase::Launching,
            FactReliability::DirectStructured,
            OwnershipClass::LauncherManaged,
        );
        let cause = guardian_test_fact(
            cause_id,
            domain,
            OperationPhase::Launching,
            FactReliability::ExactClassifier,
            OwnershipClass::LauncherManaged,
        );

        let diagnoses = diagnose(&[process, cause], OperationPhase::Launching);
        let cause_diagnosis = diagnoses
            .iter()
            .find(|diagnosis| diagnosis.id() == expected_id)
            .expect("shared cause diagnosis");
        let lifecycle = diagnoses
            .iter()
            .find(|diagnosis| diagnosis.id() == DiagnosisId::ProcessLifecycleObserved)
            .expect("independent lifecycle diagnosis");

        assert_eq!(cause_diagnosis.fact_ids(), &[cause_id]);
        assert_eq!(
            lifecycle.fact_ids(),
            &[GuardianFactId::ProcessExitedBeforeBoot]
        );
    }
}

#[test]
fn launch_conditions_are_phase_bound_and_incomplete_classification_blocks() {
    for (cause_id, domain, generic_action, diagnosis_id) in [
        (
            GuardianFactId::JavaMajorMismatch,
            GuardianDomain::Runtime,
            GuardianActionKind::Fallback,
            DiagnosisId::JavaRuntimeMajorMismatch,
        ),
        (
            GuardianFactId::JvmArgUnsupportedGc,
            GuardianDomain::Jvm,
            GuardianActionKind::Strip,
            DiagnosisId::JvmArgUnsupported,
        ),
    ] {
        let cause = guardian_test_fact(
            cause_id,
            domain,
            OperationPhase::Launching,
            FactReliability::ExactClassifier,
            OwnershipClass::LauncherManaged,
        );
        let wrong_phase_classified = guardian_test_fact(
            GuardianFactId::LaunchFailureClassified,
            GuardianDomain::Launch,
            OperationPhase::Preparing,
            FactReliability::DirectStructured,
            OwnershipClass::UserOwned,
        );
        let wrong_phase_available = guardian_test_fact(
            GuardianFactId::LaunchRuntimeFallbackAvailable,
            GuardianDomain::Launch,
            OperationPhase::Preparing,
            FactReliability::DirectStructured,
            OwnershipClass::UserOwned,
        );

        let unclassified = diagnose(
            &[cause.clone(), wrong_phase_classified, wrong_phase_available],
            OperationPhase::Launching,
        );
        let unclassified = unclassified
            .iter()
            .find(|diagnosis| diagnosis.id() == diagnosis_id)
            .expect("unclassified shared diagnosis");
        assert!(unclassified.candidate_actions().contains(&generic_action));
        assert_eq!(unclassified.fact_ids(), &[cause_id]);

        let classified = guardian_test_fact(
            GuardianFactId::LaunchFailureClassified,
            GuardianDomain::Launch,
            OperationPhase::Launching,
            FactReliability::DirectStructured,
            OwnershipClass::UserOwned,
        );
        let classified = diagnose(&[cause, classified], OperationPhase::Launching);
        let classified = classified
            .iter()
            .find(|diagnosis| diagnosis.id() == diagnosis_id)
            .expect("classified shared diagnosis");
        assert_eq!(classified.confidence(), GuardianConfidence::High);
        assert_eq!(classified.candidate_actions(), &[GuardianActionKind::Block]);
        assert_eq!(classified.fact_ids(), &[cause_id]);
    }

    let signature = guardian_test_fact(
        GuardianFactId::LauncherManagedArtifactSignatureCorruption,
        GuardianDomain::Download,
        OperationPhase::Preparing,
        FactReliability::ExactClassifier,
        OwnershipClass::LauncherManaged,
    );
    let wrong_phase_classified = guardian_test_fact(
        GuardianFactId::LaunchFailureClassified,
        GuardianDomain::Launch,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::Unknown,
    );
    assert!(
        diagnose(
            &[signature, wrong_phase_classified],
            OperationPhase::Preparing
        )
        .iter()
        .any(|diagnosis| diagnosis.id() == DiagnosisId::LauncherManagedArtifactSignatureCorrupt)
    );

    let stale_java = guardian_test_fact(
        GuardianFactId::JavaMajorMismatch,
        GuardianDomain::Runtime,
        OperationPhase::Preparing,
        FactReliability::ExactClassifier,
        OwnershipClass::LauncherManaged,
    );
    let current_classified = guardian_test_fact(
        GuardianFactId::LaunchFailureClassified,
        GuardianDomain::Launch,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );
    let current_process = guardian_test_fact(
        GuardianFactId::ProcessExitedBeforeBoot,
        GuardianDomain::Session,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );
    let current_fallback = guardian_test_fact(
        GuardianFactId::LaunchRuntimeFallbackAvailable,
        GuardianDomain::Launch,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );
    let diagnoses = diagnose(
        &[
            stale_java,
            current_classified.clone(),
            current_process,
            current_fallback,
        ],
        OperationPhase::Launching,
    );
    let java = diagnoses
        .iter()
        .find(|diagnosis| diagnosis.id() == DiagnosisId::JavaRuntimeMajorMismatch)
        .expect("stale generic Java diagnosis");
    assert_eq!(java.confidence(), GuardianConfidence::Confirmed);
    assert!(
        java.candidate_actions()
            .contains(&GuardianActionKind::Fallback)
    );
    assert_eq!(java.fact_ids(), &[GuardianFactId::JavaMajorMismatch]);

    let stale_signature = guardian_test_fact(
        GuardianFactId::LauncherManagedArtifactSignatureCorruption,
        GuardianDomain::Download,
        OperationPhase::Validating,
        FactReliability::ExactClassifier,
        OwnershipClass::LauncherManaged,
    );
    let preparing_classified = guardian_test_fact(
        GuardianFactId::LaunchFailureClassified,
        GuardianDomain::Launch,
        OperationPhase::Preparing,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );
    assert!(
        diagnose(
            &[stale_signature, preparing_classified],
            OperationPhase::Preparing
        )
        .iter()
        .any(|diagnosis| diagnosis.id() == DiagnosisId::LauncherManagedArtifactSignatureCorrupt)
    );

    let wrong_phase_stall = guardian_test_fact(
        GuardianFactId::StartupWindowExpired,
        GuardianDomain::Startup,
        OperationPhase::Preparing,
        FactReliability::ExactClassifier,
        OwnershipClass::LauncherManaged,
    );
    let launching_classified = guardian_test_fact(
        GuardianFactId::LaunchFailureClassified,
        GuardianDomain::Launch,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::LauncherManaged,
    );
    assert!(
        diagnose(
            &[wrong_phase_stall, launching_classified],
            OperationPhase::Launching
        )
        .iter()
        .all(|diagnosis| diagnosis.id() != DiagnosisId::StartupStalled)
    );
}

#[test]
fn classified_startup_context_does_not_contaminate_diagnosis_properties() {
    let cause = guardian_test_fact(
        GuardianFactId::OutOfMemory,
        GuardianDomain::Startup,
        OperationPhase::Launching,
        FactReliability::ExactClassifier,
        OwnershipClass::LauncherManaged,
    );
    let process = guardian_test_fact(
        GuardianFactId::ProcessExitedBeforeBoot,
        GuardianDomain::Session,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::Unknown,
    );
    let classified = guardian_test_fact(
        GuardianFactId::LaunchFailureClassified,
        GuardianDomain::Launch,
        OperationPhase::Launching,
        FactReliability::DirectStructured,
        OwnershipClass::UserOwned,
    );

    let diagnoses = diagnose(&[cause, process, classified], OperationPhase::Launching);

    assert_eq!(
        diagnoses
            .iter()
            .map(|diagnosis| diagnosis.id())
            .collect::<Vec<_>>(),
        vec![DiagnosisId::OutOfMemory]
    );
    let diagnosis = &diagnoses[0];
    assert_eq!(diagnosis.ownership(), OwnershipClass::LauncherManaged);
    assert_eq!(
        diagnosis.fact_ids(),
        &[
            GuardianFactId::ProcessExitedBeforeBoot,
            GuardianFactId::OutOfMemory,
        ]
    );
    assert_eq!(diagnosis.affected_targets().len(), 1);
    assert_eq!(diagnosis.affected_targets()[0].id, "out_of_memory");
}

#[test]
fn condition_only_input_falls_back_without_public_condition_evidence() {
    let facts = [
        GuardianFactId::LaunchFailureClassified,
        GuardianFactId::LaunchRuntimeFallbackAvailable,
        GuardianFactId::LaunchJvmStripAvailable,
        GuardianFactId::LaunchJvmPresetDowngradeAvailable,
    ]
    .map(|id| {
        guardian_test_fact(
            id,
            GuardianDomain::Launch,
            OperationPhase::Preparing,
            FactReliability::DirectStructured,
            OwnershipClass::UserOwned,
        )
    });

    let diagnoses = diagnose(&facts, OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(
        diagnoses[0].id(),
        DiagnosisId::UnknownFailure(OperationPhase::Preparing)
    );
    assert_eq!(diagnoses[0].ownership(), OwnershipClass::Unknown);
    assert_eq!(
        diagnoses[0].fact_ids(),
        &[GuardianFactId::NoStructuredFact(OperationPhase::Preparing)]
    );
    assert_eq!(
        diagnoses[0].affected_targets()[0].id,
        "guardian-unknown-preparing"
    );
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

    let diagnoses = diagnose(&[fact], OperationPhase::Launching);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "unknown_failure_launching");
    assert_eq!(diagnoses[0].domain(), GuardianDomain::Unknown);
    assert_eq!(diagnoses[0].confidence(), GuardianConfidence::Low);
    assert!(
        diagnoses[0]
            .candidate_actions()
            .contains(&GuardianActionKind::RecordOnly)
    );
}

#[test]
fn action_plan_representation_carries_prerequisite_metadata() {
    let target = target(
        "runtime",
        TargetKind::Runtime,
        OwnershipClass::LauncherManaged,
    );
    let fact = GuardianFact {
        operation_id: None,
        id: GuardianFactId::ManagedRuntimeCorrupt,
        domain: GuardianDomain::Runtime,
        phase: OperationPhase::Preparing,
        reliability: FactReliability::DirectStructured,
        severity: None,
        confidence: None,
        ownership: OwnershipClass::LauncherManaged,
        target: Some(target.clone()),
        fields: Vec::new(),
    };
    let diagnosis = diagnose(&[fact], OperationPhase::Preparing)
        .into_iter()
        .next()
        .expect("managed runtime diagnosis");
    let prerequisite = diagnosis.action_prerequisite();
    let plan = GuardianActionPlan::new(
        StabilizationSystem::Guardian,
        prerequisite,
        vec![GuardianAction {
            kind: GuardianActionKind::Repair,
            target: Some(target),
            reason: diagnosis.id(),
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

    let diagnoses = diagnose(&[fact], OperationPhase::Preparing);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "java_probe_failed");
    assert_eq!(
        diagnoses[0].affected_targets()[0],
        TargetDescriptor::new(
            StabilizationSystem::Guardian,
            TargetKind::Runtime,
            "guardian-runtime-preparing",
            OwnershipClass::Unknown,
        )
    );
    diagnoses[0].action_prerequisite();
}

#[test]
fn empty_fact_set_unknown_diagnosis_has_fallback_target() {
    let diagnoses = diagnose(&[], OperationPhase::Launching);

    assert_eq!(diagnoses.len(), 1);
    assert_eq!(diagnoses[0].id().as_str(), "unknown_failure_launching");
    assert_eq!(
        diagnoses[0].fact_ids(),
        vec![GuardianFactId::NoStructuredFact(OperationPhase::Launching)]
    );
    assert_eq!(
        diagnoses[0].affected_targets()[0],
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
        safety_case.diagnoses[0].id().as_str(),
        "java_runtime_major_mismatch"
    );
}

#[test]
fn would_block_file_error_reaches_managed_block_policy() {
    let target = target(
        "managed_artifact",
        TargetKind::Artifact,
        OwnershipClass::LauncherManaged,
    );
    let execution_fact =
        crate::execution::file::io_error_fact(std::io::ErrorKind::WouldBlock, None, &target);
    assert_eq!(execution_fact.kind, ExecutionFactKind::FileLocked);

    let fact = guardian_fact_from_execution(&execution_fact, OperationPhase::Validating);
    assert_eq!(fact.id, GuardianFactId::FilesystemLocked);
    let safety_case = build_safety_case(
        None,
        GuardianMode::Managed,
        OperationPhase::Validating,
        &[fact],
    );
    assert_eq!(safety_case.diagnoses.len(), 1);
    assert_eq!(safety_case.diagnoses[0].id(), DiagnosisId::FilesystemLocked);

    let decision = decide_guardian_policy(
        &safety_case,
        super::GuardianPolicyContext::current_operation(),
    );
    assert_eq!(decision.kind(), GuardianActionKind::Block);
    let copy = author_guardian_copy(GuardianCopyRequest::install_failure(
        DiagnosisId::FilesystemLocked,
        decision.kind(),
        &[],
    ))
    .expect("filesystem lock copy");
    assert_eq!(
        copy.summary(),
        "Guardian blocked install because a launcher-managed file is in use."
    );
    assert_eq!(
        copy.guidance(),
        ["Close apps that may be using launcher files, then retry the install."]
    );
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
