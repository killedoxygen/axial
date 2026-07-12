use super::{
    FactReliability, GuardianActionKind, GuardianCopyRequest, GuardianDecision, GuardianDirective,
    GuardianDomain, GuardianFact, GuardianFactId, GuardianManagedJavaReason, GuardianMode,
    GuardianPolicyContext, GuardianStripJvmArgsReason, GuardianUserOutcome, SafetyCase,
    author_guardian_copy, build_safety_case, decide_guardian_policy,
};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_launcher::{CrashEvidence, LaunchFailureClass};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct GuardianPrepareFailureRequest<'a> {
    pub mode: GuardianMode,
    pub failure_class: LaunchFailureClass,
    pub public_error: &'a str,
    pub requested_java_present: bool,
    pub explicit_java_override_present: bool,
    pub explicit_jvm_args_present: bool,
    pub runtime_intervention_applied: bool,
    pub raw_jvm_args_intervention_applied: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GuardianLaunchFailureOutcome {
    pub failure_class: LaunchFailureClass,
    pub safety_case: SafetyCase,
    pub guardian_decision: GuardianDecision,
    pub user_outcome: GuardianUserOutcome,
    pub directive: Option<GuardianDirective>,
}

#[derive(Clone, Debug)]
pub struct GuardianPresetAdjustmentRequest<'a> {
    pub mode: GuardianMode,
    pub requested_preset: &'a str,
    pub effective_preset: &'a str,
    pub explicit_jvm_preset_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuardianStartupFailureObservation {
    Stalled,
    Exited { failure_class: LaunchFailureClass },
}

#[derive(Clone, Debug)]
pub struct GuardianStartupFailureRequest<'a> {
    pub mode: GuardianMode,
    pub observation: GuardianStartupFailureObservation,
    pub crash_evidence: Option<&'a CrashEvidence>,
    pub target_version_id: &'a str,
    pub runtime_major: u32,
    pub requested_java_present: bool,
    pub explicit_java_override_present: bool,
    pub explicit_jvm_args_present: bool,
    pub explicit_jvm_preset_present: bool,
    pub startup_recovery_applied: bool,
    pub disable_custom_gc: bool,
    pub effective_preset: &'a str,
}

pub fn guardian_prepare_failure_outcome(
    request: GuardianPrepareFailureRequest<'_>,
) -> GuardianLaunchFailureOutcome {
    let facts = prepare_failure_facts(&request);
    let safety_case = build_safety_case(None, request.mode, OperationPhase::Preparing, &facts);
    let guardian_decision = decide_guardian_policy(
        &safety_case,
        policy_context(request_has_explicit_prepare_intent(&request)),
    );
    let directive = prepare_failure_directive(&request, &guardian_decision);
    let user_outcome = author_guardian_copy(GuardianCopyRequest::prepare_failure(
        guardian_decision.kind,
        request.failure_class,
        request.public_error,
        request.explicit_java_override_present,
        request.explicit_jvm_args_present,
        directive.as_ref(),
    ))
    .expect("launch prepare copy request is closed");

    GuardianLaunchFailureOutcome {
        failure_class: request.failure_class,
        safety_case,
        guardian_decision,
        user_outcome,
        directive,
    }
}

pub fn guardian_prelaunch_preset_adjustment_directive(
    request: GuardianPresetAdjustmentRequest<'_>,
) -> Option<GuardianDirective> {
    let (_, decision) = evaluate_preset_adjustment(&request)?;
    preset_adjustment_directive(&request, &decision)
}

fn evaluate_preset_adjustment(
    request: &GuardianPresetAdjustmentRequest<'_>,
) -> Option<(SafetyCase, GuardianDecision)> {
    let requested = request.requested_preset.trim();
    let effective = request.effective_preset.trim();
    if requested.is_empty() || requested == effective {
        return None;
    }

    let ownership = if request.explicit_jvm_preset_present {
        OwnershipClass::UserOwned
    } else {
        OwnershipClass::LauncherManaged
    };
    let facts = [launch_fact(
        GuardianFactId::JvmPresetCompatibilityAdjusted,
        GuardianDomain::Jvm,
        OperationPhase::Preparing,
        ownership,
        "jvm_preset",
    )];
    let safety_case = build_safety_case(None, request.mode, OperationPhase::Preparing, &facts);
    let decision = decide_guardian_policy(
        &safety_case,
        policy_context(request.explicit_jvm_preset_present),
    );
    Some((safety_case, decision))
}

fn preset_adjustment_directive(
    request: &GuardianPresetAdjustmentRequest<'_>,
    decision: &GuardianDecision,
) -> Option<GuardianDirective> {
    (decision.kind == GuardianActionKind::Downgrade).then(|| {
        GuardianDirective::compatibility_preset_downgrade(
            request.requested_preset,
            request.effective_preset,
        )
    })
}

#[cfg(test)]
pub(super) fn preset_adjustment_snapshot(
    request: &GuardianPresetAdjustmentRequest<'_>,
) -> Option<(SafetyCase, GuardianDecision, Option<GuardianDirective>)> {
    let (safety_case, decision) = evaluate_preset_adjustment(request)?;
    let directive = preset_adjustment_directive(request, &decision);
    Some((safety_case, decision, directive))
}

pub fn guardian_startup_failure_outcome(
    request: GuardianStartupFailureRequest<'_>,
) -> GuardianLaunchFailureOutcome {
    let failure_class = startup_failure_class(request.observation);
    let recovery_options = startup_recovery_options(&request, failure_class);
    let facts = startup_failure_facts(&request, failure_class, &recovery_options);
    let safety_case = build_safety_case(None, request.mode, OperationPhase::Launching, &facts);
    let guardian_decision = decide_guardian_policy(
        &safety_case,
        policy_context(request_has_explicit_startup_intent(&request)),
    );
    let directive = startup_failure_directive(recovery_options, &guardian_decision);
    let user_outcome = author_guardian_copy(GuardianCopyRequest::startup_failure(
        guardian_decision.kind,
        request.observation,
        request.crash_evidence,
        request.explicit_java_override_present,
        request.explicit_jvm_args_present,
        request.explicit_jvm_preset_present,
        directive.as_ref(),
    ))
    .expect("launch startup copy request is closed");

    GuardianLaunchFailureOutcome {
        failure_class,
        safety_case,
        guardian_decision,
        user_outcome,
        directive,
    }
}

pub fn is_guardian_launch_crash_class(failure_class: LaunchFailureClass) -> bool {
    matches!(
        failure_class,
        LaunchFailureClass::OutOfMemory
            | LaunchFailureClass::GraphicsDriverCrash
            | LaunchFailureClass::MissingDependency
            | LaunchFailureClass::ModTransformationFailure
            | LaunchFailureClass::ModAttributedCrash
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianObservedLaunchFailurePhase {
    BeforeBoot,
    AfterBoot,
}

pub fn conservative_launch_recovery_preset(version_id: &str, runtime_major: u32) -> String {
    if runtime_major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn prepare_failure_facts(request: &GuardianPrepareFailureRequest<'_>) -> Vec<GuardianFact> {
    let (domain, ownership, target_id) = match request.failure_class {
        LaunchFailureClass::JavaRuntimeMismatch => (
            GuardianDomain::Runtime,
            java_override_ownership(request.explicit_java_override_present),
            "explicit_java_override",
        ),
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => (
            GuardianDomain::Jvm,
            jvm_override_ownership(request.explicit_jvm_args_present),
            "explicit_jvm_args",
        ),
        _ => (
            GuardianDomain::Launch,
            OwnershipClass::LauncherManaged,
            "launch_prepare_failed",
        ),
    };
    let mut facts = vec![
        launch_fact(
            failure_class_fact_id(request.failure_class),
            domain,
            OperationPhase::Preparing,
            ownership,
            target_id,
        ),
        condition_fact(
            GuardianFactId::LaunchFailureClassified,
            OperationPhase::Preparing,
        ),
    ];
    if request.failure_class == LaunchFailureClass::JavaRuntimeMismatch
        && request.requested_java_present
        && request.explicit_java_override_present
        && !request.runtime_intervention_applied
    {
        facts.push(condition_fact(
            GuardianFactId::LaunchRuntimeFallbackAvailable,
            OperationPhase::Preparing,
        ));
    }
    if matches!(
        request.failure_class,
        LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering
    ) && request.explicit_jvm_args_present
        && !request.raw_jvm_args_intervention_applied
    {
        facts.push(condition_fact(
            GuardianFactId::LaunchJvmStripAvailable,
            OperationPhase::Preparing,
        ));
    }
    facts
}

fn startup_failure_facts(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
    recovery_options: &StartupRecoveryOptions,
) -> Vec<GuardianFact> {
    let (domain, ownership, target_id) = match failure_class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => (
            GuardianDomain::Jvm,
            jvm_startup_ownership(request),
            "startup_jvm_settings",
        ),
        LaunchFailureClass::JavaRuntimeMismatch => (
            GuardianDomain::Runtime,
            java_override_ownership(request.explicit_java_override_present),
            "startup_java_runtime",
        ),
        LaunchFailureClass::OutOfMemory
        | LaunchFailureClass::GraphicsDriverCrash
        | LaunchFailureClass::MissingDependency
        | LaunchFailureClass::ModTransformationFailure
        | LaunchFailureClass::ModAttributedCrash => (
            GuardianDomain::Startup,
            OwnershipClass::UserOwned,
            "instance_crash",
        ),
        LaunchFailureClass::LauncherManagedArtifactSignature => (
            GuardianDomain::Download,
            OwnershipClass::LauncherManaged,
            "launcher_managed_jars",
        ),
        _ => (
            GuardianDomain::Startup,
            OwnershipClass::LauncherManaged,
            "startup_monitoring",
        ),
    };

    let mut facts = Vec::new();
    if matches!(
        request.observation,
        GuardianStartupFailureObservation::Exited { .. }
    ) {
        facts.push(GuardianFact {
            operation_id: None,
            id: GuardianFactId::ProcessExitedBeforeBoot,
            domain: GuardianDomain::Session,
            phase: OperationPhase::Launching,
            reliability: FactReliability::ProcessLifecycle,
            severity: None,
            confidence: None,
            ownership: OwnershipClass::LauncherManaged,
            target: None,
            fields: Vec::new(),
        });
    }
    facts.push(launch_fact(
        failure_class_fact_id(failure_class),
        domain,
        OperationPhase::Launching,
        ownership,
        target_id,
    ));
    facts.push(condition_fact(
        GuardianFactId::LaunchFailureClassified,
        OperationPhase::Launching,
    ));
    if recovery_options.runtime_fallback.is_some() {
        facts.push(condition_fact(
            GuardianFactId::LaunchRuntimeFallbackAvailable,
            OperationPhase::Launching,
        ));
    }
    if recovery_options.jvm_preset_downgrade.is_some() {
        facts.push(condition_fact(
            GuardianFactId::LaunchJvmPresetDowngradeAvailable,
            OperationPhase::Launching,
        ));
    }
    if recovery_options.disable_custom_gc.is_some() {
        facts.push(condition_fact(
            GuardianFactId::LaunchJvmStripAvailable,
            OperationPhase::Launching,
        ));
    }
    facts
}

fn launch_fact(
    id: GuardianFactId,
    domain: GuardianDomain,
    phase: OperationPhase,
    ownership: OwnershipClass,
    target_id: &str,
) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain,
        phase,
        reliability: FactReliability::ExactClassifier,
        severity: None,
        confidence: None,
        ownership,
        target: Some(target(domain, target_id, ownership)),
        fields: Vec::new(),
    }
}

fn condition_fact(id: GuardianFactId, phase: OperationPhase) -> GuardianFact {
    GuardianFact {
        operation_id: None,
        id,
        domain: GuardianDomain::Launch,
        phase,
        reliability: FactReliability::DirectStructured,
        severity: None,
        confidence: None,
        ownership: OwnershipClass::Unknown,
        target: None,
        fields: Vec::new(),
    }
}

fn prepare_failure_directive(
    request: &GuardianPrepareFailureRequest<'_>,
    decision: &GuardianDecision,
) -> Option<GuardianDirective> {
    match (request.failure_class, decision.kind) {
        (LaunchFailureClass::JavaRuntimeMismatch, GuardianActionKind::Fallback)
            if request.requested_java_present
                && request.explicit_java_override_present
                && !request.runtime_intervention_applied =>
        {
            Some(GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::PrepareFailure,
            })
        }
        (
            LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering,
            GuardianActionKind::Strip,
        ) if request.explicit_jvm_args_present && !request.raw_jvm_args_intervention_applied => {
            Some(GuardianDirective::StripJvmArgs {
                reason: GuardianStripJvmArgsReason::PrepareFailure,
            })
        }
        _ => None,
    }
}

fn startup_failure_directive(
    recovery_options: StartupRecoveryOptions,
    decision: &GuardianDecision,
) -> Option<GuardianDirective> {
    let template = match decision.kind {
        GuardianActionKind::Fallback => recovery_options.runtime_fallback,
        GuardianActionKind::Downgrade => recovery_options.jvm_preset_downgrade,
        GuardianActionKind::Strip => recovery_options.disable_custom_gc,
        _ => None,
    }?;
    Some(template)
}

fn startup_recovery_options(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
) -> StartupRecoveryOptions {
    if request.startup_recovery_applied {
        return StartupRecoveryOptions::default();
    }

    let mut options = StartupRecoveryOptions::default();
    match failure_class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            let effective_preset = request.effective_preset.trim();
            if !effective_preset.is_empty() {
                let preset = conservative_launch_recovery_preset(
                    request.target_version_id,
                    request.runtime_major,
                );
                if !preset.is_empty() && preset != effective_preset {
                    options.jvm_preset_downgrade =
                        Some(GuardianDirective::startup_preset_downgrade(&preset));
                }
            }
            if !request.disable_custom_gc {
                options.disable_custom_gc = Some(GuardianDirective::DisableCustomGc);
            }
        }
        LaunchFailureClass::JavaRuntimeMismatch
            if request.requested_java_present && request.explicit_java_override_present =>
        {
            options.runtime_fallback = Some(GuardianDirective::UseManagedJava {
                reason: GuardianManagedJavaReason::StartupRecovery,
            });
        }
        LaunchFailureClass::OutOfMemory => {}
        _ => {}
    }
    options
}

#[derive(Clone, Debug, Default)]
struct StartupRecoveryOptions {
    runtime_fallback: Option<GuardianDirective>,
    jvm_preset_downgrade: Option<GuardianDirective>,
    disable_custom_gc: Option<GuardianDirective>,
}

fn request_has_explicit_prepare_intent(request: &GuardianPrepareFailureRequest<'_>) -> bool {
    request.explicit_java_override_present || request.explicit_jvm_args_present
}

fn request_has_explicit_startup_intent(request: &GuardianStartupFailureRequest<'_>) -> bool {
    request.explicit_java_override_present
        || request.explicit_jvm_args_present
        || request.explicit_jvm_preset_present
}

fn policy_context(explicit_user_intent: bool) -> GuardianPolicyContext {
    let context = GuardianPolicyContext::current_operation();
    if explicit_user_intent {
        context.with_explicit_user_intent()
    } else {
        context
    }
}

fn target(domain: GuardianDomain, id: &str, ownership: OwnershipClass) -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::Guardian,
        target_kind_for_domain(domain),
        id,
        ownership,
    )
}

fn target_kind_for_domain(domain: GuardianDomain) -> TargetKind {
    match domain {
        GuardianDomain::Runtime => TargetKind::Runtime,
        GuardianDomain::Jvm | GuardianDomain::Config => TargetKind::Config,
        GuardianDomain::Startup | GuardianDomain::Session | GuardianDomain::Launch => {
            TargetKind::Session
        }
        GuardianDomain::Install | GuardianDomain::Library | GuardianDomain::Download => {
            TargetKind::Artifact
        }
        GuardianDomain::Performance => TargetKind::PerformanceComposition,
        GuardianDomain::Network => TargetKind::NetworkResource,
        GuardianDomain::Filesystem => TargetKind::FilesystemPath,
        GuardianDomain::Auth => TargetKind::Account,
        GuardianDomain::State | GuardianDomain::Unknown => TargetKind::Session,
    }
}

fn java_override_ownership(explicit_java_override_present: bool) -> OwnershipClass {
    if explicit_java_override_present {
        OwnershipClass::UserOwned
    } else {
        OwnershipClass::LauncherManaged
    }
}

fn jvm_override_ownership(explicit_jvm_args_present: bool) -> OwnershipClass {
    if explicit_jvm_args_present {
        OwnershipClass::UserOwned
    } else {
        OwnershipClass::LauncherManaged
    }
}

fn jvm_startup_ownership(request: &GuardianStartupFailureRequest<'_>) -> OwnershipClass {
    if request.explicit_jvm_args_present || request.explicit_jvm_preset_present {
        OwnershipClass::UserOwned
    } else {
        OwnershipClass::LauncherManaged
    }
}

fn startup_failure_class(observation: GuardianStartupFailureObservation) -> LaunchFailureClass {
    match observation {
        GuardianStartupFailureObservation::Stalled => LaunchFailureClass::StartupStalled,
        GuardianStartupFailureObservation::Exited { failure_class } => failure_class,
    }
}

fn failure_class_fact_id(failure_class: LaunchFailureClass) -> GuardianFactId {
    match failure_class {
        LaunchFailureClass::JavaRuntimeMismatch => GuardianFactId::JavaMajorMismatch,
        LaunchFailureClass::JvmUnsupportedOption => GuardianFactId::JvmArgUnsupported,
        LaunchFailureClass::JvmExperimentalUnlock => {
            GuardianFactId::JvmArgExperimentalUnlockMissing
        }
        LaunchFailureClass::JvmOptionOrdering => GuardianFactId::JvmArgUnlockOrderInvalid,
        LaunchFailureClass::OutOfMemory => GuardianFactId::OutOfMemory,
        LaunchFailureClass::GraphicsDriverCrash => GuardianFactId::GraphicsDriverCrash,
        LaunchFailureClass::MissingDependency => GuardianFactId::MissingDependency,
        LaunchFailureClass::ModTransformationFailure => GuardianFactId::ModTransformationFailure,
        LaunchFailureClass::ModAttributedCrash => GuardianFactId::ModAttributedCrash,
        LaunchFailureClass::ClasspathModuleConflict => GuardianFactId::ClasspathModuleConflict,
        LaunchFailureClass::LauncherManagedArtifactSignature => {
            GuardianFactId::LauncherManagedArtifactSignatureCorruption
        }
        LaunchFailureClass::AuthModeIncompatible => GuardianFactId::AuthModeIncompatible,
        LaunchFailureClass::LoaderBootstrapFailure => GuardianFactId::LoaderBootstrapFailure,
        LaunchFailureClass::StartupStalled => GuardianFactId::StartupWindowExpired,
        LaunchFailureClass::Unknown => GuardianFactId::UnknownLaunchFailure,
    }
}

fn is_legacy_version_family(version_id: &str) -> bool {
    if matches!(version_id.as_bytes().first(), Some(b'a' | b'b')) {
        return true;
    }
    let numbers = version_id
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 12)
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianObservedLaunchFailurePhase, GuardianPrepareFailureRequest,
        GuardianStartupFailureObservation, GuardianStartupFailureRequest,
        guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
        guardian_startup_failure_outcome,
    };
    use crate::guardian::{
        GuardianActionKind, GuardianCopyRequest, GuardianDirective, GuardianFactId, GuardianMode,
        GuardianPresetDowngradeReason, author_guardian_copy, conservative_launch_recovery_preset,
        guardian_directive_description,
    };
    use crate::state::contracts::{OperationPhase, OwnershipClass};
    use axial_launcher::{CrashEvidence, LaunchFailureClass};

    #[test]
    fn managed_prelaunch_preset_adjustment_returns_downgrade_directive() {
        let directive = guardian_prelaunch_preset_adjustment_directive(
            super::GuardianPresetAdjustmentRequest {
                mode: GuardianMode::Managed,
                requested_preset: "ultra_low_latency",
                effective_preset: "performance",
                explicit_jvm_preset_present: true,
            },
        )
        .expect("preset directive");

        let GuardianDirective::DowngradeJvmPreset { preset, reason } = &directive else {
            panic!("expected preset downgrade directive")
        };
        assert_eq!(preset.as_str(), "performance");
        assert!(matches!(
            reason,
            GuardianPresetDowngradeReason::Compatibility { requested_preset }
                if requested_preset.as_str() == "ultra_low_latency"
        ));
        assert_eq!(
            guardian_directive_description(&directive),
            "Guardian downgraded JVM preset from \"ultra_low_latency\" to \"performance\" before launch"
        );
    }

    #[test]
    fn startup_launcher_managed_signature_corruption_blocks_with_specific_diagnosis() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::LauncherManagedArtifactSignature,
            },
            crash_evidence: None,
            target_version_id: "1.5.2",
            runtime_major: 8,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "legacy",
        });

        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Block);
        assert_eq!(
            outcome.safety_case.diagnoses[0].id().as_str(),
            "launcher_managed_artifact_signature_corrupt"
        );
        assert!(outcome.directive.is_none());
        assert!(outcome.user_outcome.details().contains(
            &"Minecraft exited before startup completed with detected launcher-managed jar signature corruption.".to_string()
        ));
        assert!(outcome.user_outcome.guidance().contains(
            &"Repair the installed version so Axial can replace the affected launcher-managed jars.".to_string()
        ));
    }

    #[test]
    fn startup_out_of_memory_blocks_with_bounded_copy_and_no_recovery() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::OutOfMemory,
            },
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });

        assert_eq!(outcome.failure_class, LaunchFailureClass::OutOfMemory);
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Block);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(
            outcome.safety_case.diagnoses[0].id().as_str(),
            "out_of_memory"
        );
        assert!(
            outcome.safety_case.diagnoses[0]
                .fact_ids()
                .contains(&GuardianFactId::OutOfMemory)
        );
        assert!(outcome.directive.is_none());
        assert_eq!(
            outcome.user_outcome.summary(),
            "Guardian blocked launch startup."
        );
        assert_eq!(
            outcome.user_outcome.details(),
            ["Minecraft exited before startup completed after running out of memory."]
        );
        assert_eq!(
            outcome.user_outcome.guidance(),
            ["Review the instance memory allocation and close memory-heavy apps before retrying."]
        );
        assert!(outcome.user_outcome.summary().chars().count() <= 180);
        assert!(
            outcome
                .user_outcome
                .details()
                .iter()
                .chain(outcome.user_outcome.guidance())
                .all(|line| line.chars().count() <= 240)
        );

        assert_eq!(
            outcome.safety_case.diagnoses[0].ownership(),
            OwnershipClass::UserOwned
        );

        let post_boot = author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
            LaunchFailureClass::OutOfMemory,
            None,
            GuardianObservedLaunchFailurePhase::AfterBoot,
        ))
        .expect("OOM is an accepted launch crash");
        assert_eq!(post_boot.decision(), GuardianActionKind::Warn);
        assert_eq!(post_boot.phase(), OperationPhase::Running);
        assert_eq!(
            post_boot.summary(),
            "Minecraft stopped after running out of memory."
        );
        assert!(post_boot.summary().chars().count() <= 180);
        assert!(
            post_boot
                .details()
                .iter()
                .chain(post_boot.guidance())
                .all(|line| line.chars().count() <= 240)
        );
    }

    #[test]
    fn accepted_crash_classes_are_user_owned_with_bounded_specific_copy() {
        for failure_class in [
            LaunchFailureClass::GraphicsDriverCrash,
            LaunchFailureClass::MissingDependency,
            LaunchFailureClass::ModTransformationFailure,
            LaunchFailureClass::ModAttributedCrash,
        ] {
            let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
                mode: GuardianMode::Managed,
                observation: GuardianStartupFailureObservation::Exited { failure_class },
                crash_evidence: None,
                target_version_id: "1.21.1",
                runtime_major: 21,
                requested_java_present: false,
                explicit_java_override_present: false,
                explicit_jvm_args_present: false,
                explicit_jvm_preset_present: false,
                startup_recovery_applied: false,
                disable_custom_gc: false,
                effective_preset: "performance",
            });

            assert_eq!(
                outcome.safety_case.diagnoses[0].id().as_str(),
                failure_class.as_str()
            );
            assert_eq!(
                outcome.safety_case.diagnoses[0].ownership(),
                OwnershipClass::UserOwned
            );
            assert!(outcome.directive.is_none());
            assert!(!outcome.user_outcome.details().is_empty());
            assert!(!outcome.user_outcome.guidance().is_empty());
            assert!(
                outcome
                    .user_outcome
                    .details()
                    .iter()
                    .chain(outcome.user_outcome.guidance())
                    .all(|line| line.chars().count() <= 240)
            );

            let before_boot = author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
                failure_class,
                None,
                GuardianObservedLaunchFailurePhase::BeforeBoot,
            ))
            .expect("accepted before-boot crash outcome");
            assert_eq!(before_boot.decision(), GuardianActionKind::Block);
            assert_eq!(before_boot.phase(), OperationPhase::Launching);
            assert_eq!(before_boot.details(), outcome.user_outcome.details());
            assert_eq!(before_boot.guidance(), outcome.user_outcome.guidance());

            let post_boot = author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
                failure_class,
                None,
                GuardianObservedLaunchFailurePhase::AfterBoot,
            ))
            .expect("accepted post-boot crash outcome");
            assert_eq!(post_boot.phase(), OperationPhase::Running);
            assert!(!post_boot.details().is_empty());
            assert!(!post_boot.guidance().is_empty());
        }
    }

    #[test]
    fn mod_attributed_copy_uses_only_bounded_typed_mod_name() {
        let crash_evidence: CrashEvidence = serde_json::from_value(serde_json::json!({
            "source": "minecraft_crash_report",
            "truncated": false,
            "suspected_mods": [{"name": "Example Machines"}],
            "names_out_of_memory": false
        }))
        .expect("typed crash evidence");
        let startup = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::ModAttributedCrash,
            },
            crash_evidence: Some(&crash_evidence),
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });
        let post_boot = author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
            LaunchFailureClass::ModAttributedCrash,
            Some(&crash_evidence),
            GuardianObservedLaunchFailurePhase::AfterBoot,
        ))
        .expect("mod-attributed post-boot outcome");

        assert!(startup.user_outcome.details()[0].contains("Example Machines"));
        assert!(startup.user_outcome.guidance()[0].contains("Example Machines"));
        assert!(post_boot.summary().contains("Example Machines"));
        assert!(post_boot.details()[0].contains("Example Machines"));
        assert!(post_boot.guidance()[0].contains("Example Machines"));
        assert!(
            author_guardian_copy(GuardianCopyRequest::observed_launch_failure(
                LaunchFailureClass::Unknown,
                None,
                GuardianObservedLaunchFailurePhase::AfterBoot,
            ))
            .is_none()
        );
    }

    #[test]
    fn startup_stall_blocks_with_guardian_authored_redacted_outcome() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Stalled,
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 21,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });

        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Block);
        assert_eq!(outcome.user_outcome.decision(), GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.summary(),
            "Guardian blocked launch startup."
        );
        assert!(outcome.directive.is_none());
        assert!(outcome.user_outcome.details().contains(
            &"No startup activity was observed before the startup window ended.".to_string()
        ));
    }

    #[test]
    fn public_prepare_outcome_redacts_unsafe_error_text() {
        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: GuardianMode::Managed,
            failure_class: LaunchFailureClass::Unknown,
            public_error: "/home/alice/.jdks/java.exe --accessToken secret -Xmx8192M --username Alice",
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: true,
            runtime_intervention_applied: true,
            raw_jvm_args_intervention_applied: true,
        });
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(
            outcome.user_outcome.summary(),
            "Guardian blocked launch preparation."
        );
        assert!(!lower.contains("/home"));
        assert!(!lower.contains("java.exe"));
        assert!(!lower.contains("accesstoken"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--username"));
        assert!(!lower.contains("secret"));
    }

    #[test]
    fn public_prepare_outcome_falls_back_when_text_exceeds_byte_bound() {
        let public_error = "é".repeat(121);
        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: GuardianMode::Managed,
            failure_class: LaunchFailureClass::Unknown,
            public_error: &public_error,
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: false,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });

        assert_eq!(
            outcome.user_outcome.details(),
            ["Launch preparation failed before Minecraft could start."]
        );
    }

    #[test]
    fn conservative_recovery_preset_matches_runtime_and_version_family() {
        assert_eq!(
            conservative_launch_recovery_preset("1.21.1", 21),
            "performance"
        );
        assert_eq!(conservative_launch_recovery_preset("1.12.2", 17), "legacy");
        assert_eq!(conservative_launch_recovery_preset("b1.7.3", 21), "legacy");
        assert_eq!(conservative_launch_recovery_preset("1.21.1", 8), "legacy");
    }
}
