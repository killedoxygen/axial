use super::{
    FactReliability, GuardianActionKind, GuardianConfidence, GuardianCopyRequest, GuardianDecision,
    GuardianDirective, GuardianDomain, GuardianFact, GuardianFactId, GuardianManagedJavaReason,
    GuardianMode, GuardianPolicyContext, GuardianSeverity, GuardianStripJvmArgsReason,
    GuardianUserOutcome, PreflightAdmission, SafetyCase, SafetyOutcome, author_guardian_copy,
    build_safety_case, decide_guardian_policy,
};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct GuardianPreflightOutcomeRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub mode: GuardianMode,
    pub phase: OperationPhase,
    pub facts: &'a [GuardianFact],
    pub readiness: GuardianPreflightReadiness<'a>,
    pub resources: GuardianPreflightResourceSignals,
    pub overrides: GuardianPreflightOverrideSignals,
    pub explicit_user_intent: bool,
}

impl<'a> GuardianPreflightOutcomeRequest<'a> {
    pub fn new(mode: GuardianMode, facts: &'a [GuardianFact]) -> Self {
        Self {
            operation_id: None,
            mode,
            phase: OperationPhase::Validating,
            facts,
            readiness: GuardianPreflightReadiness::ready(),
            resources: GuardianPreflightResourceSignals::default(),
            overrides: GuardianPreflightOverrideSignals::default(),
            explicit_user_intent: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianPreflightReadiness<'a> {
    pub launchable: bool,
    pub facts: &'a [GuardianFact],
}

impl<'a> GuardianPreflightReadiness<'a> {
    pub fn ready() -> Self {
        Self {
            launchable: true,
            facts: &[],
        }
    }

    pub fn from_facts(launchable: bool, facts: &'a [GuardianFact]) -> Self {
        Self { launchable, facts }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianPreflightResourceSignals {
    pub memory_clamped: bool,
    pub low_memory_allocation: bool,
    pub memory_pressure: bool,
    pub cpu_pressure: bool,
    pub install_pressure: bool,
    pub disk_pressure: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianPreflightOverrideSignals {
    pub explicit_java_override: bool,
    pub explicit_jvm_preset: bool,
    pub explicit_jvm_args: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GuardianPreflightOutcome {
    pub safety_case: SafetyCase,
    pub guardian_decision: GuardianDecision,
    pub safety: SafetyOutcome,
    pub user_outcome: GuardianUserOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<GuardianDirective>,
}

pub fn guardian_preflight_outcome(
    request: GuardianPreflightOutcomeRequest<'_>,
) -> GuardianPreflightOutcome {
    let operation_id = request.operation_id.cloned();
    let facts = preflight_facts(&request, operation_id.clone());
    let safety_case = build_safety_case(operation_id, request.mode, request.phase, &facts);
    let guardian_decision =
        decide_guardian_policy(&safety_case, preflight_policy_context(&request, &facts));
    let preflight_decision = preflight_boundary_verdict(guardian_decision.kind());
    let directives = preflight_directives(guardian_decision.kind());
    let diagnosis_ids = safety_case
        .diagnoses
        .iter()
        .map(|diagnosis| diagnosis.id())
        .collect::<Vec<_>>();
    let user_outcome = author_guardian_copy(GuardianCopyRequest::preflight(
        guardian_decision.kind(),
        preflight_decision,
        request.phase,
        &diagnosis_ids,
        &facts,
    ))
    .expect("preflight copy summary table covers every preflight verdict");
    let safety = SafetyOutcome {
        decision: preflight_decision,
        summary: user_outcome.summary().to_string(),
        detail: user_outcome.details().first().cloned(),
        diagnoses: guardian_decision.diagnoses().to_vec(),
    };

    GuardianPreflightOutcome {
        safety_case,
        guardian_decision,
        safety,
        user_outcome,
        directives,
    }
}

fn preflight_facts(
    request: &GuardianPreflightOutcomeRequest<'_>,
    operation_id: Option<OperationId>,
) -> Vec<GuardianFact> {
    let mut facts = Vec::new();
    for fact in request.facts.iter().chain(request.readiness.facts.iter()) {
        push_unique_fact(&mut facts, public_safe_fact(fact));
    }
    for fact in resource_signal_facts(operation_id.clone(), request.resources) {
        push_unique_fact(&mut facts, fact);
    }
    if request.mode == GuardianMode::Custom {
        for fact in override_signal_facts(operation_id, request.overrides) {
            push_unique_fact(&mut facts, fact);
        }
    }
    facts
}

fn push_unique_fact(facts: &mut Vec<GuardianFact>, fact: GuardianFact) {
    let target_id = fact.target.as_ref().map(|target| target.id.as_str());
    if facts.iter().any(|existing| {
        existing.id == fact.id
            && existing.target.as_ref().map(|target| target.id.as_str()) == target_id
    }) {
        return;
    }
    facts.push(fact);
}

fn preflight_policy_context(
    request: &GuardianPreflightOutcomeRequest<'_>,
    facts: &[GuardianFact],
) -> GuardianPolicyContext {
    let explicit_user_intent = request.explicit_user_intent
        || request.overrides.explicit_java_override
        || request.overrides.explicit_jvm_args
        || request.overrides.explicit_jvm_preset
        || facts.iter().any(|fact| {
            matches!(fact.domain, GuardianDomain::Runtime | GuardianDomain::Jvm)
                && fact.ownership == OwnershipClass::UserOwned
        });
    let context = GuardianPolicyContext::current_operation();
    let context = if explicit_user_intent {
        context.with_explicit_user_intent()
    } else {
        context
    };
    let admission = if !request.readiness.launchable
        || request
            .readiness
            .facts
            .iter()
            .any(|fact| fact.severity == Some(GuardianSeverity::Blocking))
    {
        PreflightAdmission::Blocked
    } else {
        PreflightAdmission::Ready
    };
    context.for_launch_preflight(admission, facts)
}

fn preflight_boundary_verdict(decision: GuardianActionKind) -> GuardianActionKind {
    match decision {
        GuardianActionKind::AskUser => GuardianActionKind::Block,
        decision => decision,
    }
}

fn preflight_directives(decision: GuardianActionKind) -> Vec<GuardianDirective> {
    match decision {
        GuardianActionKind::Fallback => vec![GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::Preflight,
        }],
        GuardianActionKind::Strip => vec![GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::Preflight,
        }],
        _ => Vec::new(),
    }
}

fn resource_signal_facts(
    operation_id: Option<OperationId>,
    signals: GuardianPreflightResourceSignals,
) -> Vec<GuardianFact> {
    let mut facts = Vec::new();
    if signals.memory_clamped {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::LaunchMemoryMinClamped,
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            TargetKind::Config,
            "launch_memory_settings",
        ));
    }
    if signals.low_memory_allocation {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::LaunchMemoryAllocationLow,
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            TargetKind::Config,
            "launch_memory_settings",
        ));
    }
    if signals.memory_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::LaunchResourceMemoryPressure,
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.cpu_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::LaunchResourceCpuPressure,
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.install_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::LaunchResourceInstallPressure,
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.disk_pressure {
        facts.push(signal_fact(
            operation_id,
            GuardianFactId::LaunchResourceDiskPressure,
            GuardianDomain::Filesystem,
            OwnershipClass::LauncherManaged,
            TargetKind::FilesystemPath,
            "launch_resource_budget",
        ));
    }
    facts
}

fn override_signal_facts(
    operation_id: Option<OperationId>,
    signals: GuardianPreflightOverrideSignals,
) -> Vec<GuardianFact> {
    let mut facts = Vec::new();
    if signals.explicit_java_override {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::CustomJavaOverridePresent,
            GuardianDomain::Runtime,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_java_override",
        ));
    }
    if signals.explicit_jvm_preset {
        facts.push(signal_fact(
            operation_id.clone(),
            GuardianFactId::CustomJvmPresetPresent,
            GuardianDomain::Jvm,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_jvm_preset",
        ));
    }
    if signals.explicit_jvm_args {
        facts.push(signal_fact(
            operation_id,
            GuardianFactId::CustomJvmArgsPresent,
            GuardianDomain::Jvm,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_jvm_args",
        ));
    }
    facts
}

fn signal_fact(
    operation_id: Option<OperationId>,
    id: GuardianFactId,
    domain: GuardianDomain,
    ownership: OwnershipClass,
    target_kind: TargetKind,
    target_id: &str,
) -> GuardianFact {
    GuardianFact {
        operation_id,
        id,
        domain,
        phase: OperationPhase::Validating,
        reliability: FactReliability::DirectStructured,
        severity: Some(GuardianSeverity::Warning),
        confidence: Some(GuardianConfidence::High),
        ownership,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Guardian,
            target_kind,
            target_id,
            ownership,
        )),
        fields: Vec::new(),
    }
}

fn public_safe_fact(fact: &GuardianFact) -> GuardianFact {
    GuardianFact {
        operation_id: fact.operation_id.clone(),
        id: fact.id,
        domain: fact.domain,
        phase: fact.phase,
        reliability: fact.reliability,
        severity: fact.severity,
        confidence: fact.confidence,
        ownership: fact.ownership,
        target: fact.target.as_ref().map(public_safe_target),
        fields: public_safe_fields(&fact.fields),
    }
}

fn public_safe_target(target: &TargetDescriptor) -> TargetDescriptor {
    TargetDescriptor::new(
        target.system,
        target.kind,
        public_safe_token(target.id.as_str(), "target"),
        target.ownership,
    )
}

fn public_safe_fields(fields: &[EvidenceField]) -> Vec<EvidenceField> {
    fields
        .iter()
        .filter_map(|field| {
            let key = sanitize_evidence_token(&field.key, RedactionAudience::UserVisible, 32)?;
            let value = field.value_for(RedactionAudience::UserVisible)?;
            let value = sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)?;
            Some(EvidenceField::new(key, value, EvidenceSensitivity::Public))
        })
        .collect()
}

fn public_safe_token(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPreflightOutcomeRequest, GuardianPreflightReadiness,
        GuardianPreflightResourceSignals, guardian_preflight_outcome,
    };
    use crate::guardian::{
        FactReliability, GuardianActionKind, GuardianConfidence, GuardianDirective, GuardianDomain,
        GuardianFact, GuardianFactId, GuardianMode, GuardianSeverity, GuardianStripJvmArgsReason,
        launch_failure_memory::{
            RECENT_REPAIR_FAILED_FACT_ID, RECENT_STARTUP_FAILURE_FACT_ID,
            REPAIR_SUPPRESSED_UNTIL_FACT_ID,
        },
    };
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn java_override_unavailable_blocks_when_readiness_says_launch_is_impossible() {
        let readiness_fact = fact(
            GuardianFactId::JavaOverrideMissing,
            GuardianDomain::Runtime,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_java_override",
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(false, &[readiness_fact]),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Custom, &[])
        });

        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(outcome.safety.decision, GuardianActionKind::Block);
        assert!(outcome.user_outcome.details().iter().any(|detail| detail
            == "Guardian blocked launch because the selected Java override is unavailable."));
    }

    #[test]
    fn java_override_confirmation_copy_survives_temporary_boundary_block() {
        let fact = fact(
            GuardianFactId::JavaOverrideMissing,
            GuardianDomain::Runtime,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_java_override",
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Custom, &[fact])
        });

        assert_eq!(
            outcome.guardian_decision.kind(),
            GuardianActionKind::AskUser
        );
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.summary(),
            "Guardian needs confirmation before launch."
        );
        assert!(outcome.user_outcome.guidance().contains(
            &"Confirm managed Java for this launch or choose a valid Java runtime.".to_string()
        ));
        assert!(outcome.directives.is_empty());
    }

    #[test]
    fn secondary_malformed_jvm_copy_survives_managed_java_fallback() {
        let facts = [
            fact(
                GuardianFactId::JavaOverrideMissing,
                GuardianDomain::Runtime,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_java_override",
            ),
            fact(
                GuardianFactId::JvmArgsParseFailed,
                GuardianDomain::Jvm,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_jvm_args",
            ),
        ];

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &facts,
        ));

        assert_eq!(
            outcome.guardian_decision.kind(),
            GuardianActionKind::Fallback
        );
        assert_eq!(
            outcome.user_outcome.decision(),
            GuardianActionKind::Fallback
        );
        assert_eq!(
            &outcome.user_outcome.details()[..2],
            [
                "Guardian will ignore the unavailable Java override and use managed Java for this launch.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
        assert_eq!(
            &outcome.user_outcome.guidance()[..2],
            [
                "Update or remove the bad Java override after launch if you want to use Custom Java again.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
        assert!(outcome.user_outcome.details().len() <= 6);
        assert!(outcome.user_outcome.guidance().len() <= 6);
    }

    #[test]
    fn secondary_malformed_jvm_copy_survives_custom_java_confirmation() {
        let facts = [
            fact(
                GuardianFactId::JavaOverrideMissing,
                GuardianDomain::Runtime,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_java_override",
            ),
            fact(
                GuardianFactId::JvmArgsParseFailed,
                GuardianDomain::Jvm,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_jvm_args",
            ),
        ];

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Custom, &facts)
        });

        assert_eq!(
            outcome.guardian_decision.kind(),
            GuardianActionKind::AskUser
        );
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(
            &outcome.user_outcome.details()[..2],
            [
                "Guardian needs confirmation before changing the selected Java override.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
        assert_eq!(
            &outcome.user_outcome.guidance()[..2],
            [
                "Confirm managed Java for this launch or choose a valid Java runtime.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
        assert!(outcome.user_outcome.details().len() <= 6);
        assert!(outcome.user_outcome.guidance().len() <= 6);
    }

    #[test]
    fn secondary_java_probe_copy_survives_managed_jvm_strip() {
        let facts = [
            fact(
                GuardianFactId::JavaProbeFailed,
                GuardianDomain::Runtime,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_java_override",
            ),
            fact(
                GuardianFactId::JvmArgsParseFailed,
                GuardianDomain::Jvm,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_jvm_args",
            ),
        ];

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &facts,
        ));

        assert_eq!(outcome.guardian_decision.kind(), GuardianActionKind::Strip);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Strip);
        assert_eq!(
            &outcome.user_outcome.details()[..2],
            [
                "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
                "Guardian removed malformed explicit JVM args for this launch.",
            ]
        );
        assert_eq!(
            &outcome.user_outcome.guidance()[..2],
            [
                "Use a Java runtime that can run `java -version`, or switch back to Managed Java.",
                "Fix the saved JVM args before re-enabling them.",
            ]
        );
    }

    #[test]
    fn secondary_java_probe_copy_survives_custom_jvm_warning() {
        let facts = [
            fact(
                GuardianFactId::JavaProbeFailed,
                GuardianDomain::Runtime,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_java_override",
            ),
            fact(
                GuardianFactId::JvmArgsParseFailed,
                GuardianDomain::Jvm,
                GuardianSeverity::Blocking,
                OwnershipClass::UserOwned,
                TargetKind::Config,
                "explicit_jvm_args",
            ),
        ];

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Custom,
            &facts,
        ));

        assert_eq!(outcome.guardian_decision.kind(), GuardianActionKind::Warn);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Warn);
        assert_eq!(
            &outcome.user_outcome.details()[..2],
            [
                "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
        assert_eq!(
            &outcome.user_outcome.guidance()[..2],
            [
                "Use a Java runtime that can run `java -version`, or switch back to Managed Java.",
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ]
        );
    }

    #[test]
    fn blocked_repair_diagnosis_uses_table_owned_generic_detail() {
        let runtime_fact = fact(
            GuardianFactId::ManagedRuntimeCorrupt,
            GuardianDomain::Runtime,
            GuardianSeverity::Repairable,
            OwnershipClass::LauncherManaged,
            TargetKind::Runtime,
            "managed_runtime",
        );
        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(false, &[]),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[runtime_fact])
        });

        assert_eq!(outcome.guardian_decision.kind(), GuardianActionKind::Block);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.details(),
            ["Guardian blocked launch because preflight readiness failed."]
        );
        assert_eq!(
            outcome.safety.detail.as_deref(),
            Some("Guardian blocked launch because preflight readiness failed.")
        );
    }

    #[test]
    fn readiness_admission_uses_typed_severity_without_fact_id_membership() {
        let mut readiness_fact = fact(
            GuardianFactId::ExitCodeZero,
            GuardianDomain::Session,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "session",
        );
        let blocked = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(
                true,
                std::slice::from_ref(&readiness_fact),
            ),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[])
        });
        assert_eq!(blocked.guardian_decision.kind(), GuardianActionKind::Block);
        assert_eq!(blocked.user_outcome.decision(), GuardianActionKind::Block);

        readiness_fact.severity = Some(GuardianSeverity::Warning);
        let ready = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(true, &[readiness_fact]),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[])
        });
        assert_eq!(
            ready.guardian_decision.kind(),
            GuardianActionKind::RecordOnly
        );
        assert_eq!(
            ready.user_outcome.decision(),
            GuardianActionKind::RecordOnly
        );
    }

    #[test]
    fn malformed_jvm_args_strip_in_managed_preflight_but_block_when_disabled() {
        let fact = fact(
            GuardianFactId::JvmArgsParseFailed,
            GuardianDomain::Jvm,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_jvm_args",
        );

        let managed = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(
                GuardianMode::Managed,
                std::slice::from_ref(&fact),
            )
        });
        assert_eq!(managed.guardian_decision.kind(), GuardianActionKind::Strip);
        assert_eq!(managed.user_outcome.decision(), GuardianActionKind::Strip);
        assert_eq!(
            managed.directives,
            vec![GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::Preflight,
            }]
        );
        assert!(managed.user_outcome.details().iter().any(|detail| {
            detail == "Guardian removed malformed explicit JVM args for this launch."
        }));

        let disabled = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Disabled, &[fact])
        });
        assert_eq!(disabled.user_outcome.decision(), GuardianActionKind::Block);
        assert!(disabled.directives.is_empty());
    }

    #[test]
    fn missing_launch_artifact_readiness_blocks_preflight() {
        let readiness_fact = fact(
            GuardianFactId::ClientJarMissing,
            GuardianDomain::Install,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
            "client_jar",
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(false, &[readiness_fact]),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[])
        });

        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert!(outcome.user_outcome.details().contains(
            &"Guardian blocked launch because client game files are missing.".to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Install or repair the affected version before launching again.".to_string()
        ));
    }

    #[test]
    fn launcher_managed_signature_readiness_blocks_preflight_with_specific_copy() {
        let readiness_fact = fact(
            GuardianFactId::LauncherManagedArtifactSignatureCorruption,
            GuardianDomain::Download,
            GuardianSeverity::Blocking,
            OwnershipClass::LauncherManaged,
            TargetKind::Artifact,
            "launcher_managed_jars",
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            readiness: GuardianPreflightReadiness::from_facts(false, &[readiness_fact]),
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[])
        });

        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert!(outcome.user_outcome.details().contains(
            &"Guardian blocked launch because launcher-managed jar signatures are inconsistent."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Install or repair the affected version before launching again.".to_string()
        ));
    }

    #[test]
    fn public_preflight_copy_and_summary_are_redacted() {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::JvmArgsParseFailed,
            domain: GuardianDomain::Jvm,
            phase: OperationPhase::Validating,
            reliability: FactReliability::ExactClassifier,
            severity: Some(GuardianSeverity::Blocking),
            confidence: Some(GuardianConfidence::Confirmed),
            ownership: OwnershipClass::UserOwned,
            target: Some(TargetDescriptor {
                system: StabilizationSystem::Guardian,
                kind: TargetKind::Config,
                id: r"C:\Users\Alice\.jdks\java.exe -Xmx8192M --accessToken secret".to_string(),
                ownership: OwnershipClass::UserOwned,
            }),
            fields: vec![
                EvidenceField::new(
                    "raw",
                    r#"/home/alice/.jdks/java -Xmx8192M --username Alice"#,
                    EvidenceSensitivity::Public,
                ),
                EvidenceField::new("parser", "shell_words", EvidenceSensitivity::Public),
            ],
        };

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            resources: GuardianPreflightResourceSignals {
                memory_pressure: true,
                ..GuardianPreflightResourceSignals::default()
            },
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[fact])
        });
        let encoded = serde_json::to_string(&outcome).expect("preflight outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert!(lower.contains("guardian removed malformed explicit jvm args"));
        assert!(!lower.contains("/home"));
        assert!(!lower.contains("users\\\\alice"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--username"));
        assert!(!lower.contains("accesstoken"));
        assert!(!lower.contains("secret"));
    }

    #[test]
    fn historical_launch_facts_only_warn_and_never_direct_launch_actions() {
        let historical_facts = [
            fact_with_fields(
                RECENT_STARTUP_FAILURE_FACT_ID,
                "instance-a",
                [
                    ("failure_class", "out_of_memory"),
                    ("occurrences", "1"),
                    ("latest_observed_today", "true"),
                ],
            ),
            fact_with_fields(
                RECENT_REPAIR_FAILED_FACT_ID,
                "instance-a",
                [("diagnosis", "java_runtime_recovery")],
            ),
            fact_with_fields(
                REPAIR_SUPPRESSED_UNTIL_FACT_ID,
                "instance-a",
                [("suppression_until", "2026-07-11T11:05:00Z")],
            ),
        ];

        for mode in [
            GuardianMode::Managed,
            GuardianMode::Custom,
            GuardianMode::Disabled,
        ] {
            for historical_fact in &historical_facts {
                let mut historical_fact = historical_fact.clone();
                historical_fact.severity = Some(GuardianSeverity::Blocking);
                let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
                    explicit_user_intent: true,
                    ..GuardianPreflightOutcomeRequest::new(mode, &[historical_fact])
                });

                assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Warn);
                assert!(outcome.directives.is_empty());
            }
        }
    }

    #[test]
    fn historical_warning_survives_the_producer_jvm_args_empty_diagnosis() {
        let empty_args = fact(
            GuardianFactId::JvmArgsEmpty,
            GuardianDomain::Jvm,
            GuardianSeverity::Info,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_jvm_args",
        );
        let historical = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-a",
            [
                ("failure_class", "out_of_memory"),
                ("occurrences", "1"),
                ("latest_observed_today", "true"),
            ],
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &[empty_args, historical],
        ));

        assert_eq!(outcome.safety_case.diagnoses.len(), 1);
        assert_eq!(outcome.guardian_decision.kind(), GuardianActionKind::Warn);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Warn);
        assert!(outcome.user_outcome.details().iter().any(|detail| {
            detail == "This instance has recorded one out-of-memory crash; the latest was today."
        }));
    }

    #[test]
    fn recent_startup_failure_copy_uses_truthful_occurrence_windows() {
        let today = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-a",
            [
                ("failure_class", "mod_attributed_crash"),
                ("occurrences", "4"),
                ("latest_observed_today", "true"),
                ("occurrences_today", "3"),
            ],
        );
        let recorded = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-b",
            [
                ("failure_class", "missing_dependency"),
                ("occurrences", "4"),
                ("latest_observed_today", "true"),
            ],
        );
        let recent = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-c",
            [
                ("failure_class", "graphics_driver_crash"),
                ("occurrences", "1"),
                ("latest_observed_today", "false"),
            ],
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &[today, recorded, recent],
        ));

        assert!(
            outcome
                .user_outcome
                .details()
                .contains(&"This instance had 3 mod-attributed crashes today.".to_string())
        );
        assert!(
            outcome.user_outcome.details().contains(
                &"This instance has recorded 4 missing-dependency crashes; the latest was today."
                    .to_string()
            )
        );
        assert!(outcome.user_outcome.details().contains(
            &"This instance has recorded one graphics driver crash; the latest was within the past 24 hours."
                .to_string()
        ));
    }

    #[test]
    fn oom_history_gives_concrete_budgeted_or_unverified_headroom_guidance() {
        let suggested = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-a",
            [
                ("failure_class", "out_of_memory"),
                ("occurrences", "1"),
                ("latest_observed_today", "true"),
                ("current_memory_mb", "4096"),
                ("suggested_memory_mb", "6144"),
            ],
        );
        let unverified_headroom = fact_with_fields(
            RECENT_STARTUP_FAILURE_FACT_ID,
            "instance-b",
            [
                ("failure_class", "out_of_memory"),
                ("occurrences", "1"),
                ("latest_observed_today", "true"),
                ("current_memory_mb", "4096"),
            ],
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &[suggested, unverified_headroom],
        ));

        assert!(outcome.user_outcome.guidance().contains(
            &"Increase this instance's maximum memory from 4096 MB to 6144 MB before relaunching."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Guardian could not verify safe headroom for a larger memory allocation. Close another session or free memory before relaunching."
                .to_string()
        ));
    }

    #[test]
    fn failed_repair_and_active_suppression_have_closed_copy() {
        let repair = fact_with_fields(
            RECENT_REPAIR_FAILED_FACT_ID,
            "instance-a",
            [("diagnosis", "jvm_preset_recovery")],
        );
        let suppression = fact_with_fields(
            REPAIR_SUPPRESSED_UNTIL_FACT_ID,
            "instance-a",
            [
                ("diagnosis", "jvm_preset_recovery"),
                ("suppression_until", "2026-07-11T13:45:00+02:00"),
            ],
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &[repair, suppression],
        ));

        assert!(
            outcome
                .user_outcome
                .details()
                .contains(&"The previous JVM preset recovery attempt failed.".to_string())
        );
        assert!(
            outcome
                .user_outcome
                .guidance()
                .contains(&"Review the JVM preset before relaunching.".to_string())
        );
        assert!(outcome.user_outcome.details().contains(
            &"Guardian will not auto-repair this launch again until 11:45 UTC.".to_string()
        ));
    }

    #[test]
    fn warn_copy_keeps_historical_advisories_when_detail_caps_are_saturated() {
        let historical_facts = [
            fact_with_fields(
                RECENT_STARTUP_FAILURE_FACT_ID,
                "instance-a",
                [
                    ("failure_class", "out_of_memory"),
                    ("occurrences", "2"),
                    ("latest_observed_today", "true"),
                    ("occurrences_today", "2"),
                    ("current_memory_mb", "4096"),
                    ("suggested_memory_mb", "6144"),
                ],
            ),
            fact_with_fields(
                REPAIR_SUPPRESSED_UNTIL_FACT_ID,
                "instance-a",
                [("suppression_until", "2026-07-11T13:45:00+02:00")],
            ),
        ];
        let resources = GuardianPreflightResourceSignals {
            memory_clamped: true,
            low_memory_allocation: true,
            memory_pressure: true,
            cpu_pressure: true,
            install_pressure: true,
            disk_pressure: true,
        };

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            resources,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &historical_facts)
        });

        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Warn);
        assert_eq!(outcome.user_outcome.details().len(), 6);
        assert_eq!(outcome.user_outcome.guidance().len(), 6);
        assert!(
            outcome
                .user_outcome
                .details()
                .contains(&"This instance had 2 out-of-memory crashes today.".to_string())
        );
        assert!(outcome.user_outcome.details().contains(
            &"Guardian will not auto-repair this launch again until 11:45 UTC.".to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Increase this instance's maximum memory from 4096 MB to 6144 MB before relaunching."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Review the launch settings before retrying; unchanged settings will not trigger another automatic repair before 11:45 UTC."
                .to_string()
        ));
        assert!(
            !outcome
                .user_outcome
                .details()
                .contains(&"Launch-relevant storage has low free space.".to_string())
        );
    }

    #[test]
    fn historical_copy_is_redacted_and_bounded_under_hostile_fields() {
        let mut facts = [
            "out_of_memory",
            "graphics_driver_crash",
            "missing_dependency",
            "mod_transformation_failure",
            "mod_attributed_crash",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, failure_class)| {
            fact_with_fields(
                RECENT_STARTUP_FAILURE_FACT_ID,
                &format!("instance-{index}"),
                [
                    ("failure_class", failure_class),
                    ("occurrences", "4294967295"),
                    ("latest_observed_today", "true"),
                    ("raw", "/home/alice/java -Xmx8192M --accessToken secret"),
                ],
            )
        })
        .collect::<Vec<_>>();
        for (index, diagnosis) in [
            "java_runtime_recovery",
            "jvm_arg_unsupported",
            "jvm_preset_recovery",
        ]
        .into_iter()
        .enumerate()
        {
            facts.push(fact_with_fields(
                RECENT_REPAIR_FAILED_FACT_ID,
                &format!("repair-{index}"),
                [("diagnosis", diagnosis)],
            ));
        }

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest::new(
            GuardianMode::Managed,
            &facts,
        ));
        let encoded = serde_json::to_string(&outcome).expect("historical outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert!(outcome.user_outcome.details().len() <= 6);
        assert!(outcome.user_outcome.guidance().len() <= 6);
        assert!(
            outcome
                .user_outcome
                .details()
                .iter()
                .chain(outcome.user_outcome.guidance())
                .all(|line| line.chars().count() <= 240)
        );
        for sensitive in ["/home", "alice", "-xmx", "accesstoken", "secret"] {
            assert!(!lower.contains(sensitive), "leaked {sensitive}: {encoded}");
        }
    }

    fn fact(
        id: GuardianFactId,
        domain: GuardianDomain,
        severity: GuardianSeverity,
        ownership: OwnershipClass,
        kind: TargetKind,
        target_id: &str,
    ) -> GuardianFact {
        GuardianFact {
            operation_id: None,
            id,
            domain,
            phase: OperationPhase::Validating,
            reliability: FactReliability::DirectStructured,
            severity: Some(severity),
            confidence: Some(GuardianConfidence::Confirmed),
            ownership,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Guardian,
                kind,
                target_id,
                ownership,
            )),
            fields: Vec::new(),
        }
    }

    fn fact_with_fields<const N: usize>(
        id: GuardianFactId,
        target_id: &str,
        fields: [(&str, &str); N],
    ) -> GuardianFact {
        let mut fact = fact(
            id,
            GuardianDomain::Launch,
            GuardianSeverity::Warning,
            OwnershipClass::LauncherManaged,
            TargetKind::Instance,
            target_id,
        );
        fact.fields = fields
            .into_iter()
            .map(|(key, value)| EvidenceField::new(key, value, EvidenceSensitivity::Public))
            .collect();
        fact
    }
}
