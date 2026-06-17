use super::{
    FactReliability, GuardianConfidence, GuardianDecision, GuardianDecisionKind, GuardianDomain,
    GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext, GuardianSeverity,
    GuardianUserOutcome, SafetyCase, SafetyOutcome, build_safety_case, decide_guardian_policy,
};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_text,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use serde::{Deserialize, Serialize};

const MAX_PREFLIGHT_DETAILS: usize = 6;
const MAX_PREFLIGHT_GUIDANCE: usize = 6;
const MAX_PREFLIGHT_SUMMARY_CHARS: usize = 180;
const MAX_PREFLIGHT_DETAIL_CHARS: usize = 240;

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuardianPreflightOutcome {
    pub safety_case: SafetyCase,
    pub guardian_decision: GuardianDecision,
    pub safety: SafetyOutcome,
    pub user_outcome: GuardianUserOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directives: Vec<GuardianPreflightDirective>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianPreflightDirective {
    UseManagedJavaForAttempt,
    StripExplicitJvmArgsForAttempt,
}

pub fn guardian_preflight_outcome(
    request: GuardianPreflightOutcomeRequest<'_>,
) -> GuardianPreflightOutcome {
    let operation_id = request.operation_id.as_ref().map(public_safe_operation_id);
    let facts = preflight_facts(&request, operation_id.clone());
    let safety_case = build_safety_case(operation_id, request.mode, request.phase, &facts);
    let guardian_decision =
        decide_guardian_policy(&safety_case, preflight_policy_context(&request, &facts));
    let preflight_decision = preflight_decision_kind(&request, &facts, &guardian_decision);
    let directives = preflight_directives(preflight_decision, &safety_case);
    let (details, guidance) = preflight_copy(preflight_decision, &safety_case);
    let summary = public_text(
        preflight_summary(preflight_decision),
        MAX_PREFLIGHT_SUMMARY_CHARS,
    )
    .unwrap_or_else(|| "Guardian recorded launch preflight readiness.".to_string());

    let user_outcome = GuardianUserOutcome {
        decision: preflight_decision,
        phase: request.phase,
        summary: summary.clone(),
        details,
        guidance,
    };
    let safety = SafetyOutcome {
        decision: preflight_decision,
        summary,
        detail: user_outcome.details.first().cloned(),
        diagnoses: guardian_decision.diagnoses.clone(),
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
    if explicit_user_intent {
        context.with_explicit_user_intent()
    } else {
        context
    }
}

fn preflight_decision_kind(
    request: &GuardianPreflightOutcomeRequest<'_>,
    facts: &[GuardianFact],
    decision: &GuardianDecision,
) -> GuardianDecisionKind {
    if readiness_blocks_launch(request.readiness) || decision.kind == GuardianDecisionKind::Block {
        return GuardianDecisionKind::Block;
    }
    if request.mode == GuardianMode::Managed
        && decision.kind == GuardianDecisionKind::Fallback
        && facts.iter().any(is_java_override_unavailable_fact)
    {
        return GuardianDecisionKind::Fallback;
    }
    if request.mode == GuardianMode::Managed
        && decision.kind == GuardianDecisionKind::Strip
        && facts.iter().any(is_jvm_preflight_strip_fact)
    {
        return GuardianDecisionKind::Strip;
    }
    if decision.kind == GuardianDecisionKind::AskUser {
        if decision
            .diagnoses
            .iter()
            .any(|diagnosis| diagnosis.as_str() == "java_override_unavailable")
        {
            return GuardianDecisionKind::AskUser;
        }
        if facts.iter().any(is_preflight_warning_fact) {
            return GuardianDecisionKind::Warn;
        }
        return GuardianDecisionKind::AskUser;
    }
    if facts.iter().any(is_preflight_warning_fact) {
        return GuardianDecisionKind::Warn;
    }
    decision.kind
}

fn preflight_directives(
    decision: GuardianDecisionKind,
    safety_case: &SafetyCase,
) -> Vec<GuardianPreflightDirective> {
    let mut directives = Vec::new();
    if decision == GuardianDecisionKind::Fallback
        && safety_case
            .diagnoses
            .iter()
            .any(|diagnosis| java_fallback_diagnosis(diagnosis.id.as_str()))
    {
        directives.push(GuardianPreflightDirective::UseManagedJavaForAttempt);
    }
    if decision == GuardianDecisionKind::Strip
        && safety_case.diagnoses.iter().any(|diagnosis| {
            matches!(
                diagnosis.id.as_str(),
                "jvm_args_malformed" | "jvm_arg_unsupported" | "jvm_arg_unsafe_override"
            )
        })
    {
        directives.push(GuardianPreflightDirective::StripExplicitJvmArgsForAttempt);
    }
    directives
}

fn java_fallback_diagnosis(diagnosis_id: &str) -> bool {
    matches!(
        diagnosis_id,
        "java_override_unavailable"
            | "java_probe_failed"
            | "java_runtime_major_mismatch"
            | "java_runtime_update_too_old"
    )
}

fn is_java_override_unavailable_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id.as_str(),
        "java_override_missing"
            | "java_override_undefined_sentinel"
            | "java_probe_failed"
            | "java_major_mismatch"
            | "java_update_too_old"
    )
}

fn is_jvm_preflight_strip_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id.as_str(),
        "jvm_args_parse_failed"
            | "jvm_arg_reserved_launcher_flag"
            | "jvm_arg_memory_conflict"
            | "jvm_arg_unsupported_gc"
            | "jvm_arg_unlock_order_invalid"
            | "jvm_arg_unsafe_classpath_override"
            | "jvm_arg_unsafe_native_path_override"
            | "jvm_arg_agent_override"
    )
}

fn readiness_blocks_launch(readiness: GuardianPreflightReadiness<'_>) -> bool {
    !readiness.launchable
        || readiness.facts.iter().any(|fact| {
            fact.severity == Some(GuardianSeverity::Blocking) && is_readiness_fact(fact.id.as_str())
        })
}

fn is_readiness_fact(id: &str) -> bool {
    matches!(
        id,
        "version_json_missing"
            | "parent_version_missing"
            | "incomplete_install"
            | "client_jar_missing"
            | "libraries_missing"
            | "asset_index_missing"
            | "managed_runtime_missing"
            | "java_override_missing"
    )
}

fn is_preflight_warning_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id.as_str(),
        "java_override_empty"
            | "java_override_undefined_sentinel"
            | "java_override_missing"
            | "jvm_args_parse_failed"
            | "jvm_arg_reserved_launcher_flag"
            | "jvm_arg_memory_conflict"
            | "jvm_arg_unsupported_gc"
            | "jvm_arg_unlock_order_invalid"
            | "jvm_arg_unsafe_classpath_override"
            | "jvm_arg_unsafe_native_path_override"
            | "jvm_arg_agent_override"
            | "launch_memory_min_clamped"
            | "launch_memory_allocation_low"
            | "launch_resource_memory_pressure"
            | "launch_resource_cpu_pressure"
            | "launch_resource_install_pressure"
            | "launch_resource_disk_pressure"
            | "custom_java_override_present"
            | "custom_jvm_preset_present"
            | "custom_jvm_args_present"
    )
}

fn preflight_copy(
    decision: GuardianDecisionKind,
    safety_case: &SafetyCase,
) -> (Vec<String>, Vec<String>) {
    let mut details = Vec::new();
    let mut guidance = Vec::new();
    for diagnosis in &safety_case.diagnoses {
        if let Some(detail) = detail_for_diagnosis(diagnosis.id.as_str(), decision) {
            push_unique_public(&mut details, detail, MAX_PREFLIGHT_DETAILS);
        }
        if let Some(value) = guidance_for_diagnosis(diagnosis.id.as_str(), decision) {
            push_unique_public(&mut guidance, value, MAX_PREFLIGHT_GUIDANCE);
        }
    }
    if details.is_empty() && decision == GuardianDecisionKind::Block {
        push_unique_public(
            &mut details,
            "Guardian blocked launch because preflight readiness failed.",
            MAX_PREFLIGHT_DETAILS,
        );
    }
    (details, guidance)
}

fn detail_for_diagnosis(
    diagnosis_id: &str,
    decision: GuardianDecisionKind,
) -> Option<&'static str> {
    match diagnosis_id {
        "java_override_unavailable" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Guardian will ignore the unavailable Java override and use managed Java for this launch.",
            ),
            GuardianDecisionKind::Block => {
                Some("Guardian blocked launch because the selected Java override is unavailable.")
            }
            GuardianDecisionKind::AskUser => {
                Some("Guardian needs confirmation before changing the selected Java override.")
            }
            _ => Some(
                "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        "java_probe_failed" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Guardian will ignore the Java override that failed probing and use managed Java for this launch.",
            ),
            GuardianDecisionKind::Block => Some(
                "Guardian blocked launch because the selected Java override could not be probed.",
            ),
            GuardianDecisionKind::AskUser => Some(
                "Guardian needs confirmation before bypassing a Java override that could not be probed.",
            ),
            _ => Some(
                "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        "java_runtime_major_mismatch" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Guardian will ignore the incompatible Java override and use managed Java for this launch.",
            ),
            GuardianDecisionKind::Block => Some(
                "Guardian blocked launch because the selected Java override has the wrong Java version.",
            ),
            GuardianDecisionKind::AskUser => {
                Some("Guardian needs confirmation before bypassing an incompatible Java override.")
            }
            _ => Some(
                "Guardian detected a Java override that does not match the version requirement.",
            ),
        },
        "java_runtime_update_too_old" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Guardian will ignore the outdated Java override and use managed Java for this launch.",
            ),
            GuardianDecisionKind::Block => {
                Some("Guardian blocked launch because the selected Java 8 override is too old.")
            }
            GuardianDecisionKind::AskUser => {
                Some("Guardian needs confirmation before bypassing an outdated Java override.")
            }
            _ => Some("Guardian detected a Java 8 override that is too old for this launch."),
        },
        "jvm_args_malformed" => match decision {
            GuardianDecisionKind::Strip => {
                Some("Guardian removed malformed explicit JVM args for this launch.")
            }
            _ => Some(
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ),
        },
        "jvm_arg_unsupported" => match decision {
            GuardianDecisionKind::Strip => {
                Some("Guardian removed unsupported explicit JVM args for this launch.")
            }
            _ => Some(
                "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
            ),
        },
        "jvm_arg_unsafe_override" => match decision {
            GuardianDecisionKind::Strip => Some(
                "Guardian removed explicit JVM args that override launcher-owned settings for this launch.",
            ),
            _ => Some(
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
            ),
        },
        "installed_version_metadata_missing" => {
            Some("Guardian blocked launch because installed version metadata is missing.")
        }
        "parent_version_metadata_missing" => {
            Some("Guardian blocked launch because parent version metadata is missing.")
        }
        "install_incomplete" => Some("Guardian blocked launch because the install is incomplete."),
        "client_jar_missing" => {
            Some("Guardian blocked launch because client game files are missing.")
        }
        "libraries_missing" => {
            Some("Guardian blocked launch because required libraries are missing.")
        }
        "asset_index_missing" => {
            Some("Guardian blocked launch because the asset index is missing.")
        }
        "launcher_managed_artifact_corrupt" => {
            Some("Guardian blocked launch because launcher-managed game files are corrupt.")
        }
        "managed_runtime_missing" => {
            Some("Managed Java runtime is missing and can be prepared before launch.")
        }
        "launch_memory_min_clamped" => Some(
            "Minimum memory was higher than maximum memory, so Croopor clamped the launch minimum to match the maximum allocation.",
        ),
        "launch_memory_allocation_low" => {
            Some("Launch memory allocation is very low for Minecraft.")
        }
        "launch_resource_memory_pressure" => {
            Some("Launch memory budget is tight for the current active sessions.")
        }
        "launch_resource_cpu_pressure" => Some(
            "Launch concurrency may be tight: other active launch sessions can saturate low-end CPUs.",
        ),
        "launch_resource_install_pressure" => {
            Some("Active install or download work may add pressure during startup.")
        }
        "launch_resource_disk_pressure" => Some("Launch-relevant storage has low free space."),
        "custom_java_override_present" => {
            Some("Guardian Custom mode will keep the selected Java override for this launch.")
        }
        "custom_jvm_preset_present" => {
            Some("Guardian Custom mode will keep the selected JVM preset for this launch.")
        }
        "custom_jvm_args_present" => Some(
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.",
        ),
        _ => None,
    }
}

fn guidance_for_diagnosis(
    diagnosis_id: &str,
    decision: GuardianDecisionKind,
) -> Option<&'static str> {
    match diagnosis_id {
        "java_override_unavailable" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Update or remove the bad Java override after launch if you want to use Custom Java again.",
            ),
            GuardianDecisionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose a valid Java runtime.")
            }
            _ => Some(
                "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        "java_probe_failed" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Update or remove the Java override after launch if you want to use Custom Java again.",
            ),
            GuardianDecisionKind::AskUser => Some(
                "Confirm managed Java for this launch or choose a Java runtime that can be probed.",
            ),
            _ => Some(
                "Use a Java runtime that can run `java -version`, or switch back to Managed Java.",
            ),
        },
        "java_runtime_major_mismatch" => match decision {
            GuardianDecisionKind::Fallback => Some(
                "Choose a Java runtime matching this Minecraft version before re-enabling the override.",
            ),
            GuardianDecisionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose a compatible Java runtime.")
            }
            _ => Some("Choose a Java runtime matching this Minecraft version requirement."),
        },
        "java_runtime_update_too_old" => match decision {
            GuardianDecisionKind::Fallback => {
                Some("Use Java 8u312 or newer before re-enabling this override.")
            }
            GuardianDecisionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose Java 8u312 or newer.")
            }
            _ => Some("Use Java 8u312 or newer for this legacy launch."),
        },
        "jvm_args_malformed" => match decision {
            GuardianDecisionKind::Strip => Some("Fix the saved JVM args before re-enabling them."),
            _ => Some(
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ),
        },
        "jvm_arg_unsupported" => match decision {
            GuardianDecisionKind::Strip => Some(
                "Use JVM flags supported by the selected Java runtime before re-enabling them.",
            ),
            _ => Some(
                "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
            ),
        },
        "jvm_arg_unsafe_override" => match decision {
            GuardianDecisionKind::Strip => Some(
                "Remove memory, classpath, native-path, or agent overrides from saved JVM args before re-enabling them.",
            ),
            _ => Some(
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
            ),
        },
        "installed_version_metadata_missing"
        | "parent_version_metadata_missing"
        | "install_incomplete"
        | "client_jar_missing"
        | "libraries_missing"
        | "asset_index_missing"
        | "launcher_managed_artifact_corrupt" => {
            Some("Install or repair the affected version before launching again.")
        }
        "managed_runtime_missing" => {
            Some("Let Croopor prepare the managed Java runtime before launching.")
        }
        "launch_memory_min_clamped" => Some(
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ),
        "launch_memory_allocation_low" => Some(
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.",
        ),
        "launch_resource_memory_pressure" => {
            Some("Close another running session or lower memory allocation if startup is unstable.")
        }
        "launch_resource_cpu_pressure" => Some(
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
        ),
        "launch_resource_install_pressure" => {
            Some("Wait for active install or download work to finish if startup feels slow.")
        }
        "launch_resource_disk_pressure" => {
            Some("Free disk space before launching if caches or natives become unreliable.")
        }
        "custom_java_override_present"
        | "custom_jvm_preset_present"
        | "custom_jvm_args_present" => {
            Some("Switch Guardian back to Managed if you want Croopor to adjust unsafe choices.")
        }
        _ => None,
    }
}

fn preflight_summary(decision: GuardianDecisionKind) -> &'static str {
    match decision {
        GuardianDecisionKind::Allow | GuardianDecisionKind::RecordOnly => {
            "Guardian recorded launch preflight readiness."
        }
        GuardianDecisionKind::Warn => "Guardian found launch preflight warnings.",
        GuardianDecisionKind::AskUser => "Guardian needs confirmation before launch.",
        GuardianDecisionKind::Block => "Guardian blocked launch preflight.",
        GuardianDecisionKind::Fallback | GuardianDecisionKind::Strip => {
            "Guardian adjusted launch preflight."
        }
        _ => "Guardian selected a guarded launch preflight action.",
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
            "launch_memory_min_clamped",
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            TargetKind::Config,
            "launch_memory_settings",
        ));
    }
    if signals.low_memory_allocation {
        facts.push(signal_fact(
            operation_id.clone(),
            "launch_memory_allocation_low",
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            TargetKind::Config,
            "launch_memory_settings",
        ));
    }
    if signals.memory_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            "launch_resource_memory_pressure",
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.cpu_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            "launch_resource_cpu_pressure",
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.install_pressure {
        facts.push(signal_fact(
            operation_id.clone(),
            "launch_resource_install_pressure",
            GuardianDomain::Performance,
            OwnershipClass::LauncherManaged,
            TargetKind::Session,
            "launch_resource_budget",
        ));
    }
    if signals.disk_pressure {
        facts.push(signal_fact(
            operation_id,
            "launch_resource_disk_pressure",
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
            "custom_java_override_present",
            GuardianDomain::Runtime,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_java_override",
        ));
    }
    if signals.explicit_jvm_preset {
        facts.push(signal_fact(
            operation_id.clone(),
            "custom_jvm_preset_present",
            GuardianDomain::Jvm,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_jvm_preset",
        ));
    }
    if signals.explicit_jvm_args {
        facts.push(signal_fact(
            operation_id,
            "custom_jvm_args_present",
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
    id: &str,
    domain: GuardianDomain,
    ownership: OwnershipClass,
    target_kind: TargetKind,
    target_id: &str,
) -> GuardianFact {
    GuardianFact {
        operation_id,
        id: GuardianFactId::new(id),
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
        operation_id: fact.operation_id.as_ref().map(public_safe_operation_id),
        id: GuardianFactId::new(public_safe_token(fact.id.as_str(), "unknown_fact")),
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

fn public_safe_operation_id(operation_id: &OperationId) -> OperationId {
    OperationId::new(public_safe_token(operation_id.as_str(), "operation"))
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

fn public_text(value: &str, max_chars: usize) -> Option<String> {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars)
}

fn push_unique_public(values: &mut Vec<String>, value: &str, max_values: usize) {
    if values.len() >= max_values {
        return;
    }
    let Some(value) = public_text(value, MAX_PREFLIGHT_DETAIL_CHARS) else {
        return;
    };
    if value.is_empty() || values.iter().any(|existing| existing == &value) {
        return;
    }
    values.push(value);
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianPreflightOutcomeRequest, GuardianPreflightReadiness,
        GuardianPreflightResourceSignals, guardian_preflight_outcome,
    };
    use crate::guardian::{
        FactReliability, GuardianConfidence, GuardianDecisionKind, GuardianDomain, GuardianFact,
        GuardianFactId, GuardianMode, GuardianPreflightDirective, GuardianSeverity,
    };
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };

    #[test]
    fn java_override_unavailable_blocks_when_readiness_says_launch_is_impossible() {
        let readiness_fact = fact(
            "java_override_missing",
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

        assert_eq!(outcome.user_outcome.decision, GuardianDecisionKind::Block);
        assert_eq!(outcome.safety.decision, GuardianDecisionKind::Block);
        assert!(outcome.user_outcome.details.iter().any(|detail| detail
            == "Guardian blocked launch because the selected Java override is unavailable."));
    }

    #[test]
    fn java_override_unavailable_asks_in_custom_when_intent_can_be_confirmed() {
        let fact = fact(
            "java_override_missing",
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
            outcome.guardian_decision.kind,
            GuardianDecisionKind::AskUser
        );
        assert_eq!(outcome.user_outcome.decision, GuardianDecisionKind::AskUser);
        assert!(outcome.user_outcome.guidance.contains(
            &"Confirm managed Java for this launch or choose a valid Java runtime.".to_string()
        ));
        assert!(outcome.directives.is_empty());
    }

    #[test]
    fn java_override_unavailable_directs_managed_java_for_managed_attempt() {
        let fact = fact(
            "java_override_missing",
            GuardianDomain::Runtime,
            GuardianSeverity::Blocking,
            OwnershipClass::UserOwned,
            TargetKind::Config,
            "explicit_java_override",
        );

        let outcome = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Managed, &[fact])
        });

        assert_eq!(
            outcome.user_outcome.decision,
            GuardianDecisionKind::Fallback
        );
        assert_eq!(
            outcome.directives,
            vec![GuardianPreflightDirective::UseManagedJavaForAttempt]
        );
    }

    #[test]
    fn malformed_jvm_args_strip_in_managed_preflight_but_block_when_disabled() {
        let fact = fact(
            "jvm_args_parse_failed",
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
        assert_eq!(managed.guardian_decision.kind, GuardianDecisionKind::Strip);
        assert_eq!(managed.user_outcome.decision, GuardianDecisionKind::Strip);
        assert_eq!(
            managed.directives,
            vec![GuardianPreflightDirective::StripExplicitJvmArgsForAttempt]
        );
        assert!(managed.user_outcome.details.iter().any(|detail| {
            detail == "Guardian removed malformed explicit JVM args for this launch."
        }));

        let disabled = guardian_preflight_outcome(GuardianPreflightOutcomeRequest {
            explicit_user_intent: true,
            ..GuardianPreflightOutcomeRequest::new(GuardianMode::Disabled, &[fact])
        });
        assert_eq!(disabled.user_outcome.decision, GuardianDecisionKind::Block);
        assert!(disabled.directives.is_empty());
    }

    #[test]
    fn missing_launch_artifact_readiness_blocks_preflight() {
        let readiness_fact = fact(
            "client_jar_missing",
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

        assert_eq!(outcome.user_outcome.decision, GuardianDecisionKind::Block);
        assert!(outcome.user_outcome.details.contains(
            &"Guardian blocked launch because client game files are missing.".to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
            &"Install or repair the affected version before launching again.".to_string()
        ));
    }

    #[test]
    fn public_preflight_copy_and_summary_are_redacted() {
        let fact = GuardianFact {
            operation_id: None,
            id: GuardianFactId::new("jvm_args_parse_failed"),
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

    fn fact(
        id: &str,
        domain: GuardianDomain,
        severity: GuardianSeverity,
        ownership: OwnershipClass,
        kind: TargetKind,
        target_id: &str,
    ) -> GuardianFact {
        GuardianFact {
            operation_id: None,
            id: GuardianFactId::new(id),
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
}
