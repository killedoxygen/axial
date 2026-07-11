use super::{
    DiagnosisId, FactReliability, GuardianActionKind, GuardianConfidence, GuardianDecision,
    GuardianDomain, GuardianFact, GuardianFactId, GuardianMode, GuardianPolicyContext,
    GuardianSeverity, GuardianUserOutcome, SafetyCase, SafetyOutcome, build_safety_case,
    decide_guardian_policy,
    launch_failure_memory::{
        RECENT_REPAIR_FAILED_FACT_ID, RECENT_STARTUP_FAILURE_FACT_ID,
        REPAIR_SUPPRESSED_UNTIL_FACT_ID,
    },
};
use crate::observability::{
    EvidenceField, EvidenceSensitivity, RedactionAudience, sanitize_evidence_text,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    OperationId, OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_launcher::LaunchFailureClass;
use chrono::{DateTime, Timelike, Utc};
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
    let (details, guidance) = preflight_copy(preflight_decision, &safety_case, &facts);
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
) -> GuardianActionKind {
    if readiness_blocks_launch(request.readiness) || decision.kind == GuardianActionKind::Block {
        return GuardianActionKind::Block;
    }
    if request.mode == GuardianMode::Managed
        && decision.kind == GuardianActionKind::Fallback
        && facts.iter().any(is_java_override_unavailable_fact)
    {
        return GuardianActionKind::Fallback;
    }
    if request.mode == GuardianMode::Managed
        && decision.kind == GuardianActionKind::Strip
        && facts.iter().any(is_jvm_preflight_strip_fact)
    {
        return GuardianActionKind::Strip;
    }
    if decision.kind == GuardianActionKind::AskUser {
        if decision
            .diagnoses
            .contains(&DiagnosisId::JavaOverrideUnavailable)
        {
            return GuardianActionKind::AskUser;
        }
        if facts.iter().any(is_preflight_warning_fact) {
            return GuardianActionKind::Warn;
        }
        return GuardianActionKind::AskUser;
    }
    if facts.iter().any(is_preflight_warning_fact) {
        return GuardianActionKind::Warn;
    }
    decision.kind
}

fn preflight_directives(
    decision: GuardianActionKind,
    safety_case: &SafetyCase,
) -> Vec<GuardianPreflightDirective> {
    let mut directives = Vec::new();
    if decision == GuardianActionKind::Fallback
        && safety_case
            .diagnoses
            .iter()
            .any(|diagnosis| java_fallback_diagnosis(diagnosis.id))
    {
        directives.push(GuardianPreflightDirective::UseManagedJavaForAttempt);
    }
    if decision == GuardianActionKind::Strip
        && safety_case.diagnoses.iter().any(|diagnosis| {
            matches!(
                diagnosis.id,
                DiagnosisId::JvmArgsMalformed
                    | DiagnosisId::JvmArgUnsupported
                    | DiagnosisId::JvmArgUnsafeOverride
            )
        })
    {
        directives.push(GuardianPreflightDirective::StripExplicitJvmArgsForAttempt);
    }
    directives
}

fn java_fallback_diagnosis(diagnosis_id: DiagnosisId) -> bool {
    matches!(
        diagnosis_id,
        DiagnosisId::JavaOverrideUnavailable
            | DiagnosisId::JavaProbeFailed
            | DiagnosisId::JavaRuntimeMajorMismatch
            | DiagnosisId::JavaRuntimeUpdateTooOld
    )
}

fn is_java_override_unavailable_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id,
        GuardianFactId::JavaOverrideMissing
            | GuardianFactId::JavaOverrideUndefinedSentinel
            | GuardianFactId::JavaProbeFailed
            | GuardianFactId::JavaMajorMismatch
            | GuardianFactId::JavaUpdateTooOld
    )
}

fn is_jvm_preflight_strip_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id,
        GuardianFactId::JvmArgsParseFailed
            | GuardianFactId::JvmArgReservedLauncherFlag
            | GuardianFactId::JvmArgMemoryConflict
            | GuardianFactId::JvmArgUnsupportedGc
            | GuardianFactId::JvmArgUnlockOrderInvalid
            | GuardianFactId::JvmArgUnsafeClasspathOverride
            | GuardianFactId::JvmArgUnsafeNativePathOverride
            | GuardianFactId::JvmArgAgentOverride
    )
}

fn readiness_blocks_launch(readiness: GuardianPreflightReadiness<'_>) -> bool {
    !readiness.launchable
        || readiness.facts.iter().any(|fact| {
            fact.severity == Some(GuardianSeverity::Blocking) && is_readiness_fact(fact.id)
        })
}

const READINESS_FACT_IDS: &[GuardianFactId] = &[
    GuardianFactId::VersionJsonMissing,
    GuardianFactId::ParentVersionMissing,
    GuardianFactId::IncompleteInstall,
    GuardianFactId::ClientJarMissing,
    GuardianFactId::LibrariesMissing,
    GuardianFactId::AssetIndexMissing,
    GuardianFactId::LauncherManagedArtifactSignatureCorruption,
    GuardianFactId::ManagedRuntimeMissing,
    GuardianFactId::JavaOverrideMissing,
];

fn is_readiness_fact(id: GuardianFactId) -> bool {
    READINESS_FACT_IDS.contains(&id)
}

fn is_preflight_warning_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id,
        GuardianFactId::JavaOverrideEmpty
            | GuardianFactId::JavaOverrideUndefinedSentinel
            | GuardianFactId::JavaOverrideMissing
            | GuardianFactId::JvmArgsParseFailed
            | GuardianFactId::JvmArgReservedLauncherFlag
            | GuardianFactId::JvmArgMemoryConflict
            | GuardianFactId::JvmArgUnsupportedGc
            | GuardianFactId::JvmArgUnlockOrderInvalid
            | GuardianFactId::JvmArgUnsafeClasspathOverride
            | GuardianFactId::JvmArgUnsafeNativePathOverride
            | GuardianFactId::JvmArgAgentOverride
            | GuardianFactId::LaunchMemoryMinClamped
            | GuardianFactId::LaunchMemoryAllocationLow
            | GuardianFactId::LaunchResourceMemoryPressure
            | GuardianFactId::LaunchResourceCpuPressure
            | GuardianFactId::LaunchResourceInstallPressure
            | GuardianFactId::LaunchResourceDiskPressure
            | GuardianFactId::CustomJavaOverridePresent
            | GuardianFactId::CustomJvmPresetPresent
            | GuardianFactId::CustomJvmArgsPresent
            | RECENT_STARTUP_FAILURE_FACT_ID
            | RECENT_REPAIR_FAILED_FACT_ID
            | REPAIR_SUPPRESSED_UNTIL_FACT_ID
    )
}

fn preflight_copy(
    decision: GuardianActionKind,
    safety_case: &SafetyCase,
    facts: &[GuardianFact],
) -> (Vec<String>, Vec<String>) {
    let mut details = Vec::new();
    let mut guidance = Vec::new();
    if decision == GuardianActionKind::Warn {
        push_historical_launch_copy(facts, &mut details, &mut guidance);
    }
    for diagnosis in &safety_case.diagnoses {
        if let Some(detail) = detail_for_diagnosis(diagnosis.id, decision) {
            push_unique_public(&mut details, detail, MAX_PREFLIGHT_DETAILS);
        }
        if let Some(value) = guidance_for_diagnosis(diagnosis.id, decision) {
            push_unique_public(&mut guidance, value, MAX_PREFLIGHT_GUIDANCE);
        }
    }
    if decision != GuardianActionKind::Warn {
        push_historical_launch_copy(facts, &mut details, &mut guidance);
    }
    if details.is_empty() && decision == GuardianActionKind::Block {
        push_unique_public(
            &mut details,
            "Guardian blocked launch because preflight readiness failed.",
            MAX_PREFLIGHT_DETAILS,
        );
    }
    (details, guidance)
}

fn push_historical_launch_copy(
    facts: &[GuardianFact],
    details: &mut Vec<String>,
    guidance: &mut Vec<String>,
) {
    for fact in facts.iter().filter(|fact| is_historical_launch_fact(fact)) {
        if let Some(detail) = historical_launch_detail(fact) {
            push_unique_public(details, &detail, MAX_PREFLIGHT_DETAILS);
        }
        if let Some(value) = historical_launch_guidance(fact) {
            push_unique_public(guidance, &value, MAX_PREFLIGHT_GUIDANCE);
        }
    }
}

fn is_historical_launch_fact(fact: &GuardianFact) -> bool {
    matches!(
        fact.id,
        RECENT_STARTUP_FAILURE_FACT_ID
            | RECENT_REPAIR_FAILED_FACT_ID
            | REPAIR_SUPPRESSED_UNTIL_FACT_ID
    )
}

fn historical_launch_detail(fact: &GuardianFact) -> Option<String> {
    if fact.id == RECENT_STARTUP_FAILURE_FACT_ID {
        recent_startup_failure_detail(fact)
    } else if fact.id == RECENT_REPAIR_FAILED_FACT_ID {
        repair_failure_copy(fact).map(|copy| copy.0.to_string())
    } else if fact.id == REPAIR_SUPPRESSED_UNTIL_FACT_ID {
        suppression_time_utc(fact)
            .map(|time| format!("Guardian will not auto-repair this launch again until {time}."))
    } else {
        None
    }
}

fn historical_launch_guidance(fact: &GuardianFact) -> Option<String> {
    if fact.id == RECENT_STARTUP_FAILURE_FACT_ID {
        recent_startup_failure_guidance(fact)
    } else if fact.id == RECENT_REPAIR_FAILED_FACT_ID {
        repair_failure_copy(fact).map(|copy| copy.1.to_string())
    } else if fact.id == REPAIR_SUPPRESSED_UNTIL_FACT_ID {
        suppression_time_utc(fact).map(|time| {
            format!(
                "Review the launch settings before retrying; unchanged settings will not trigger another automatic repair before {time}."
            )
        })
    } else {
        None
    }
}

fn recent_startup_failure_detail(fact: &GuardianFact) -> Option<String> {
    let failure_class =
        fact_field(fact, "failure_class").and_then(LaunchFailureClass::from_name)?;
    let label = launch_failure_plain_label(failure_class)?;
    let occurrences = fact_field_u32(fact, "occurrences").filter(|count| *count > 0);
    let latest_today = fact_field(fact, "latest_observed_today") == Some("true");
    let occurrences_today = latest_today
        .then(|| fact_field_u32(fact, "occurrences_today"))
        .flatten()
        .filter(|count| *count > 0)
        .filter(|count| occurrences.is_none_or(|total| *count <= total));

    Some(if let Some(count) = occurrences_today {
        counted_failure_copy("had", count, label, " today")
    } else if let Some(count) = occurrences {
        let recency = if latest_today {
            "; the latest was today"
        } else {
            "; the latest was within the past 24 hours"
        };
        counted_failure_copy("has recorded", count, label, recency)
    } else {
        format!("A recent launch ended with {}.", label.with_article)
    })
}

fn counted_failure_copy(
    verb: &str,
    count: u32,
    label: LaunchFailurePlainLabel,
    suffix: &str,
) -> String {
    if count == 1 {
        format!("This instance {verb} one {}{suffix}.", label.singular)
    } else {
        format!("This instance {verb} {count} {}{suffix}.", label.plural)
    }
}

fn recent_startup_failure_guidance(fact: &GuardianFact) -> Option<String> {
    let failure_class =
        fact_field(fact, "failure_class").and_then(LaunchFailureClass::from_name)?;
    match failure_class {
        LaunchFailureClass::OutOfMemory => {
            let current = fact_field_u32(fact, "current_memory_mb").filter(|value| *value > 0);
            let suggested = fact_field_u32(fact, "suggested_memory_mb").filter(|value| *value > 0);
            match (current, suggested) {
                (Some(current), Some(suggested)) if suggested > current => Some(format!(
                    "Increase this instance's maximum memory from {current} MB to {suggested} MB before relaunching."
                )),
                _ => Some(
                    "Guardian could not verify safe headroom for a larger memory allocation. Close another session or free memory before relaunching."
                        .to_string(),
                ),
            }
        }
        LaunchFailureClass::GraphicsDriverCrash => Some(
            "Update the graphics driver and remove graphics overrides before relaunching."
                .to_string(),
        ),
        LaunchFailureClass::MissingDependency => {
            Some("Repair the instance dependencies before relaunching.".to_string())
        }
        LaunchFailureClass::ModTransformationFailure => Some(
            "Review recently changed mods and their loader compatibility before relaunching."
                .to_string(),
        ),
        LaunchFailureClass::ModAttributedCrash => Some(
            "Review recently changed mods and disable the suspected mod before relaunching."
                .to_string(),
        ),
        LaunchFailureClass::Unknown
        | LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering
        | LaunchFailureClass::JavaRuntimeMismatch
        | LaunchFailureClass::ClasspathModuleConflict
        | LaunchFailureClass::LauncherManagedArtifactSignature
        | LaunchFailureClass::AuthModeIncompatible
        | LaunchFailureClass::LoaderBootstrapFailure
        | LaunchFailureClass::StartupStalled => None,
    }
}

#[derive(Clone, Copy)]
struct LaunchFailurePlainLabel {
    singular: &'static str,
    plural: &'static str,
    with_article: &'static str,
}

fn launch_failure_plain_label(
    failure_class: LaunchFailureClass,
) -> Option<LaunchFailurePlainLabel> {
    let label = match failure_class {
        LaunchFailureClass::OutOfMemory => LaunchFailurePlainLabel {
            singular: "out-of-memory crash",
            plural: "out-of-memory crashes",
            with_article: "an out-of-memory crash",
        },
        LaunchFailureClass::GraphicsDriverCrash => LaunchFailurePlainLabel {
            singular: "graphics driver crash",
            plural: "graphics driver crashes",
            with_article: "a graphics driver crash",
        },
        LaunchFailureClass::MissingDependency => LaunchFailurePlainLabel {
            singular: "missing-dependency crash",
            plural: "missing-dependency crashes",
            with_article: "a missing-dependency crash",
        },
        LaunchFailureClass::ModTransformationFailure => LaunchFailurePlainLabel {
            singular: "mod transformation crash",
            plural: "mod transformation crashes",
            with_article: "a mod transformation crash",
        },
        LaunchFailureClass::ModAttributedCrash => LaunchFailurePlainLabel {
            singular: "mod-attributed crash",
            plural: "mod-attributed crashes",
            with_article: "a mod-attributed crash",
        },
        LaunchFailureClass::Unknown
        | LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering
        | LaunchFailureClass::JavaRuntimeMismatch
        | LaunchFailureClass::ClasspathModuleConflict
        | LaunchFailureClass::LauncherManagedArtifactSignature
        | LaunchFailureClass::AuthModeIncompatible
        | LaunchFailureClass::LoaderBootstrapFailure
        | LaunchFailureClass::StartupStalled => return None,
    };
    Some(label)
}

fn repair_failure_copy(fact: &GuardianFact) -> Option<(&'static str, &'static str)> {
    match fact_field(fact, "diagnosis")? {
        value if value == DiagnosisId::JavaRuntimeRecovery.as_str() => Some((
            "The previous managed Java recovery attempt failed.",
            "Review the selected Java runtime before relaunching.",
        )),
        value if value == DiagnosisId::JvmArgUnsupported.as_str() => Some((
            "The previous JVM argument recovery attempt failed.",
            "Review or remove explicit JVM arguments before relaunching.",
        )),
        value if value == DiagnosisId::JvmPresetRecovery.as_str() => Some((
            "The previous JVM preset recovery attempt failed.",
            "Review the JVM preset before relaunching.",
        )),
        _ => None,
    }
}

fn suppression_time_utc(fact: &GuardianFact) -> Option<String> {
    let timestamp = fact_field(fact, "suppression_until")?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp).ok()?;
    let utc = timestamp.with_timezone(&Utc);
    Some(format!("{:02}:{:02} UTC", utc.hour(), utc.minute()))
}

fn fact_field<'a>(fact: &'a GuardianFact, key: &str) -> Option<&'a str> {
    let mut values = fact
        .fields
        .iter()
        .filter(|field| field.key == key)
        .filter_map(|field| field.value_for(RedactionAudience::UserVisible));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn fact_field_u32(fact: &GuardianFact, key: &str) -> Option<u32> {
    fact_field(fact, key)?.parse().ok()
}

fn detail_for_diagnosis(
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
) -> Option<&'static str> {
    match diagnosis_id {
        DiagnosisId::JavaOverrideUnavailable => match decision {
            GuardianActionKind::Fallback => Some(
                "Guardian will ignore the unavailable Java override and use managed Java for this launch.",
            ),
            GuardianActionKind::Block => {
                Some("Guardian blocked launch because the selected Java override is unavailable.")
            }
            GuardianActionKind::AskUser => {
                Some("Guardian needs confirmation before changing the selected Java override.")
            }
            _ => Some(
                "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        DiagnosisId::JavaProbeFailed => match decision {
            GuardianActionKind::Fallback => Some(
                "Guardian will ignore the Java override that failed probing and use managed Java for this launch.",
            ),
            GuardianActionKind::Block => Some(
                "Guardian blocked launch because the selected Java override could not be probed.",
            ),
            GuardianActionKind::AskUser => Some(
                "Guardian needs confirmation before bypassing a Java override that could not be probed.",
            ),
            _ => Some(
                "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        DiagnosisId::JavaRuntimeMajorMismatch => match decision {
            GuardianActionKind::Fallback => Some(
                "Guardian will ignore the incompatible Java override and use managed Java for this launch.",
            ),
            GuardianActionKind::Block => Some(
                "Guardian blocked launch because the selected Java override has the wrong Java version.",
            ),
            GuardianActionKind::AskUser => {
                Some("Guardian needs confirmation before bypassing an incompatible Java override.")
            }
            _ => Some(
                "Guardian detected a Java override that does not match the version requirement.",
            ),
        },
        DiagnosisId::JavaRuntimeUpdateTooOld => match decision {
            GuardianActionKind::Fallback => Some(
                "Guardian will ignore the outdated Java override and use managed Java for this launch.",
            ),
            GuardianActionKind::Block => {
                Some("Guardian blocked launch because the selected Java 8 override is too old.")
            }
            GuardianActionKind::AskUser => {
                Some("Guardian needs confirmation before bypassing an outdated Java override.")
            }
            _ => Some("Guardian detected a Java 8 override that is too old for this launch."),
        },
        DiagnosisId::JvmArgsMalformed => match decision {
            GuardianActionKind::Strip => {
                Some("Guardian removed malformed explicit JVM args for this launch.")
            }
            _ => Some(
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ),
        },
        DiagnosisId::JvmArgUnsupported => match decision {
            GuardianActionKind::Strip => {
                Some("Guardian removed unsupported explicit JVM args for this launch.")
            }
            _ => Some(
                "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
            ),
        },
        DiagnosisId::JvmArgUnsafeOverride => match decision {
            GuardianActionKind::Strip => Some(
                "Guardian removed explicit JVM args that override launcher-owned settings for this launch.",
            ),
            _ => Some(
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
            ),
        },
        DiagnosisId::InstalledVersionMetadataMissing => {
            Some("Guardian blocked launch because installed version metadata is missing.")
        }
        DiagnosisId::ParentVersionMetadataMissing => {
            Some("Guardian blocked launch because parent version metadata is missing.")
        }
        DiagnosisId::InstallIncomplete => {
            Some("Guardian blocked launch because the install is incomplete.")
        }
        DiagnosisId::ClientJarMissing => {
            Some("Guardian blocked launch because client game files are missing.")
        }
        DiagnosisId::LibrariesMissing => {
            Some("Guardian blocked launch because required libraries are missing.")
        }
        DiagnosisId::AssetIndexMissing => {
            Some("Guardian blocked launch because the asset index is missing.")
        }
        DiagnosisId::LauncherManagedArtifactCorrupt => {
            Some("Guardian blocked launch because launcher-managed game files are corrupt.")
        }
        DiagnosisId::LauncherManagedArtifactSignatureCorrupt => Some(
            "Guardian blocked launch because launcher-managed jar signatures are inconsistent.",
        ),
        DiagnosisId::ManagedRuntimeMissing => {
            Some("Managed Java runtime is missing and can be prepared before launch.")
        }
        DiagnosisId::LaunchMemoryMinClamped => Some(
            "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        ),
        DiagnosisId::LaunchMemoryAllocationLow => {
            Some("Launch memory allocation is very low for Minecraft.")
        }
        DiagnosisId::LaunchResourceMemoryPressure => {
            Some("Launch memory budget is tight for the current active sessions.")
        }
        DiagnosisId::LaunchResourceCpuPressure => Some(
            "Launch concurrency may be tight: other active launch sessions can saturate low-end CPUs.",
        ),
        DiagnosisId::LaunchResourceInstallPressure => {
            Some("Active install or download work may add pressure during startup.")
        }
        DiagnosisId::LaunchResourceDiskPressure => {
            Some("Launch-relevant storage has low free space.")
        }
        DiagnosisId::CustomJavaOverridePresent => {
            Some("Guardian Custom mode will keep the selected Java override for this launch.")
        }
        DiagnosisId::CustomJvmPresetPresent => {
            Some("Guardian Custom mode will keep the selected JVM preset for this launch.")
        }
        DiagnosisId::CustomJvmArgsPresent => Some(
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.",
        ),
        _ => None,
    }
}

fn guidance_for_diagnosis(
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
) -> Option<&'static str> {
    match diagnosis_id {
        DiagnosisId::JavaOverrideUnavailable => match decision {
            GuardianActionKind::Fallback => Some(
                "Update or remove the bad Java override after launch if you want to use Custom Java again.",
            ),
            GuardianActionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose a valid Java runtime.")
            }
            _ => Some(
                "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
            ),
        },
        DiagnosisId::JavaProbeFailed => match decision {
            GuardianActionKind::Fallback => Some(
                "Update or remove the Java override after launch if you want to use Custom Java again.",
            ),
            GuardianActionKind::AskUser => Some(
                "Confirm managed Java for this launch or choose a Java runtime that can be probed.",
            ),
            _ => Some(
                "Use a Java runtime that can run `java -version`, or switch back to Managed Java.",
            ),
        },
        DiagnosisId::JavaRuntimeMajorMismatch => match decision {
            GuardianActionKind::Fallback => Some(
                "Choose a Java runtime matching this Minecraft version before re-enabling the override.",
            ),
            GuardianActionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose a compatible Java runtime.")
            }
            _ => Some("Choose a Java runtime matching this Minecraft version requirement."),
        },
        DiagnosisId::JavaRuntimeUpdateTooOld => match decision {
            GuardianActionKind::Fallback => {
                Some("Use Java 8u312 or newer before re-enabling this override.")
            }
            GuardianActionKind::AskUser => {
                Some("Confirm managed Java for this launch or choose Java 8u312 or newer.")
            }
            _ => Some("Use Java 8u312 or newer for this legacy launch."),
        },
        DiagnosisId::JvmArgsMalformed => match decision {
            GuardianActionKind::Strip => Some("Fix the saved JVM args before re-enabling them."),
            _ => Some(
                "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
            ),
        },
        DiagnosisId::JvmArgUnsupported => match decision {
            GuardianActionKind::Strip => Some(
                "Use JVM flags supported by the selected Java runtime before re-enabling them.",
            ),
            _ => Some(
                "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
            ),
        },
        DiagnosisId::JvmArgUnsafeOverride => match decision {
            GuardianActionKind::Strip => Some(
                "Remove memory, classpath, native-path, or agent overrides from saved JVM args before re-enabling them.",
            ),
            _ => Some(
                "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
            ),
        },
        DiagnosisId::InstalledVersionMetadataMissing
        | DiagnosisId::ParentVersionMetadataMissing
        | DiagnosisId::InstallIncomplete
        | DiagnosisId::ClientJarMissing
        | DiagnosisId::LibrariesMissing
        | DiagnosisId::AssetIndexMissing
        | DiagnosisId::LauncherManagedArtifactCorrupt
        | DiagnosisId::LauncherManagedArtifactSignatureCorrupt => {
            Some("Install or repair the affected version before launching again.")
        }
        DiagnosisId::ManagedRuntimeMissing => {
            Some("Let Axial prepare the managed Java runtime before launching.")
        }
        DiagnosisId::LaunchMemoryMinClamped => Some(
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ),
        DiagnosisId::LaunchMemoryAllocationLow => Some(
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.",
        ),
        DiagnosisId::LaunchResourceMemoryPressure => {
            Some("Close another running session or lower memory allocation if startup is unstable.")
        }
        DiagnosisId::LaunchResourceCpuPressure => Some(
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
        ),
        DiagnosisId::LaunchResourceInstallPressure => {
            Some("Wait for active install or download work to finish if startup feels slow.")
        }
        DiagnosisId::LaunchResourceDiskPressure => {
            Some("Free disk space before launching if caches or natives become unreliable.")
        }
        DiagnosisId::CustomJavaOverridePresent
        | DiagnosisId::CustomJvmPresetPresent
        | DiagnosisId::CustomJvmArgsPresent => {
            Some("Switch Guardian back to Managed if you want Axial to adjust unsafe choices.")
        }
        _ => None,
    }
}

fn preflight_summary(decision: GuardianActionKind) -> &'static str {
    match decision {
        GuardianActionKind::Allow | GuardianActionKind::RecordOnly => {
            "Guardian recorded launch preflight readiness."
        }
        GuardianActionKind::Warn => "Guardian found launch preflight warnings.",
        GuardianActionKind::AskUser => "Guardian needs confirmation before launch.",
        GuardianActionKind::Block => "Guardian blocked launch preflight.",
        GuardianActionKind::Fallback | GuardianActionKind::Strip => {
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
        operation_id: fact.operation_id.as_ref().map(public_safe_operation_id),
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
        GuardianPreflightResourceSignals, RECENT_REPAIR_FAILED_FACT_ID,
        RECENT_STARTUP_FAILURE_FACT_ID, REPAIR_SUPPRESSED_UNTIL_FACT_ID,
        guardian_preflight_outcome, launch_failure_plain_label,
    };
    use crate::guardian::{
        FactReliability, GuardianActionKind, GuardianConfidence, GuardianDomain, GuardianFact,
        GuardianFactId, GuardianMode, GuardianPreflightDirective, GuardianSeverity,
        is_guardian_launch_crash_class,
    };
    use crate::observability::{EvidenceField, EvidenceSensitivity};
    use crate::state::contracts::{
        OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_launcher::LaunchFailureClass;

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

        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Block);
        assert_eq!(outcome.safety.decision, GuardianActionKind::Block);
        assert!(outcome.user_outcome.details.iter().any(|detail| detail
            == "Guardian blocked launch because the selected Java override is unavailable."));
    }

    #[test]
    fn java_override_unavailable_asks_in_custom_when_intent_can_be_confirmed() {
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

        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::AskUser);
        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::AskUser);
        assert!(outcome.user_outcome.guidance.contains(
            &"Confirm managed Java for this launch or choose a valid Java runtime.".to_string()
        ));
        assert!(outcome.directives.is_empty());
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
        assert_eq!(managed.guardian_decision.kind, GuardianActionKind::Strip);
        assert_eq!(managed.user_outcome.decision, GuardianActionKind::Strip);
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
        assert_eq!(disabled.user_outcome.decision, GuardianActionKind::Block);
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

        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Block);
        assert!(outcome.user_outcome.details.contains(
            &"Guardian blocked launch because client game files are missing.".to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
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

        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Block);
        assert!(outcome.user_outcome.details.contains(
            &"Guardian blocked launch because launcher-managed jar signatures are inconsistent."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
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

                assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Warn);
                assert!(outcome.directives.is_empty());
            }
        }
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
                .details
                .contains(&"This instance had 3 mod-attributed crashes today.".to_string())
        );
        assert!(
            outcome.user_outcome.details.contains(
                &"This instance has recorded 4 missing-dependency crashes; the latest was today."
                    .to_string()
            )
        );
        assert!(outcome.user_outcome.details.contains(
            &"This instance has recorded one graphics driver crash; the latest was within the past 24 hours."
                .to_string()
        ));
    }

    #[test]
    fn launch_failure_plain_labels_cover_exactly_guardian_crash_classes() {
        for failure_class in [
            LaunchFailureClass::Unknown,
            LaunchFailureClass::JvmUnsupportedOption,
            LaunchFailureClass::JvmExperimentalUnlock,
            LaunchFailureClass::JvmOptionOrdering,
            LaunchFailureClass::JavaRuntimeMismatch,
            LaunchFailureClass::OutOfMemory,
            LaunchFailureClass::GraphicsDriverCrash,
            LaunchFailureClass::MissingDependency,
            LaunchFailureClass::ModTransformationFailure,
            LaunchFailureClass::ModAttributedCrash,
            LaunchFailureClass::ClasspathModuleConflict,
            LaunchFailureClass::LauncherManagedArtifactSignature,
            LaunchFailureClass::AuthModeIncompatible,
            LaunchFailureClass::LoaderBootstrapFailure,
            LaunchFailureClass::StartupStalled,
        ] {
            assert_eq!(
                launch_failure_plain_label(failure_class).is_some(),
                is_guardian_launch_crash_class(failure_class),
                "plain label coverage diverged for {}",
                failure_class.as_str()
            );
        }
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

        assert!(outcome.user_outcome.guidance.contains(
            &"Increase this instance's maximum memory from 4096 MB to 6144 MB before relaunching."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
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
                .details
                .contains(&"The previous JVM preset recovery attempt failed.".to_string())
        );
        assert!(
            outcome
                .user_outcome
                .guidance
                .contains(&"Review the JVM preset before relaunching.".to_string())
        );
        assert!(outcome.user_outcome.details.contains(
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

        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Warn);
        assert_eq!(outcome.user_outcome.details.len(), 6);
        assert_eq!(outcome.user_outcome.guidance.len(), 6);
        assert!(
            outcome
                .user_outcome
                .details
                .contains(&"This instance had 2 out-of-memory crashes today.".to_string())
        );
        assert!(outcome.user_outcome.details.contains(
            &"Guardian will not auto-repair this launch again until 11:45 UTC.".to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
            &"Increase this instance's maximum memory from 4096 MB to 6144 MB before relaunching."
                .to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
            &"Review the launch settings before retrying; unchanged settings will not trigger another automatic repair before 11:45 UTC."
                .to_string()
        ));
        assert!(
            !outcome
                .user_outcome
                .details
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

        assert!(outcome.user_outcome.details.len() <= 6);
        assert!(outcome.user_outcome.guidance.len() <= 6);
        assert!(
            outcome
                .user_outcome
                .details
                .iter()
                .chain(&outcome.user_outcome.guidance)
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
