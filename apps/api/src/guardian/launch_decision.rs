use super::{
    Diagnosis, DiagnosisId, GuardianActionKind, GuardianConfidence, GuardianDecision,
    GuardianDomain, GuardianFactId, GuardianImpactVector, GuardianLaunchRecoveryDirective,
    GuardianLaunchRecoveryEffect, GuardianLaunchRecoveryKind, GuardianMode, GuardianPolicyContext,
    GuardianSeverity, GuardianUserOutcome, SafetyCase, decide_guardian_policy,
};
use crate::observability::{RedactionAudience, sanitize_evidence_text};
use crate::state::contracts::{
    OperationPhase, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
};
use axial_launcher::{CrashEvidence, LaunchFailureClass};
use serde::{Deserialize, Serialize};

const MAX_LAUNCH_DECISION_SUMMARY_CHARS: usize = 180;
const MAX_LAUNCH_DECISION_DETAIL_CHARS: usize = 240;
const MAX_LAUNCH_DECISION_LINES: usize = 6;

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuardianLaunchFailureOutcome {
    pub failure_class: LaunchFailureClass,
    pub safety_case: SafetyCase,
    pub guardian_decision: GuardianDecision,
    pub user_outcome: GuardianUserOutcome,
    pub directive: Option<GuardianLaunchRecoveryDirective>,
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
    let diagnosis = prepare_failure_diagnosis(&request);
    let safety_case = safety_case(request.mode, OperationPhase::Preparing, diagnosis);
    let guardian_decision = decide_guardian_policy(
        &safety_case,
        policy_context(request_has_explicit_prepare_intent(&request)),
    );
    let directive = prepare_failure_directive(&request, &guardian_decision);
    let user_outcome =
        prepare_failure_user_outcome(&request, &guardian_decision, directive.as_ref());

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
) -> Option<GuardianLaunchRecoveryDirective> {
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
    let diagnosis = Diagnosis {
        id: DiagnosisId::new("jvm_preset_adjusted"),
        domain: GuardianDomain::Jvm,
        severity: GuardianSeverity::Recoverable,
        confidence: GuardianConfidence::Confirmed,
        ownership,
        phase: OperationPhase::Preparing,
        fact_ids: vec![GuardianFactId::JvmPresetCompatibilityAdjusted],
        affected_targets: vec![target(GuardianDomain::Jvm, "jvm_preset", ownership)],
        impact: GuardianImpactVector {
            launchability_impact: 0.65,
            user_intent_impact: 0.45,
            ..GuardianImpactVector::default()
        },
        candidate_actions: vec![
            GuardianActionKind::Downgrade,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ],
        public_reason_template: "jvm_preset_adjusted".to_string(),
    };
    let safety_case = safety_case(request.mode, OperationPhase::Preparing, diagnosis);
    let decision = decide_guardian_policy(
        &safety_case,
        policy_context(request.explicit_jvm_preset_present),
    );
    (decision.kind == GuardianActionKind::Downgrade).then(|| {
        let requested = safe_preset_label(requested);
        let effective = safe_preset_label(effective);
        GuardianLaunchRecoveryDirective {
            kind: GuardianLaunchRecoveryKind::DowngradePreset,
            effect: GuardianLaunchRecoveryEffect::DowngradePreset {
                preset: effective.clone(),
            },
            description: format!(
                "Guardian downgraded JVM preset from \"{requested}\" to \"{effective}\" before launch"
            ),
        }
    })
}

pub fn guardian_startup_failure_outcome(
    request: GuardianStartupFailureRequest<'_>,
) -> GuardianLaunchFailureOutcome {
    let failure_class = startup_failure_class(request.observation);
    let recovery_template = startup_recovery_template(&request, failure_class);
    let diagnosis = startup_failure_diagnosis(&request, failure_class, recovery_template.as_ref());
    let safety_case = safety_case(request.mode, OperationPhase::Launching, diagnosis);
    let guardian_decision = decide_guardian_policy(
        &safety_case,
        policy_context(request_has_explicit_startup_intent(&request)),
    );
    let directive = startup_failure_directive(recovery_template, &guardian_decision);
    let user_outcome = startup_failure_user_outcome(
        &request,
        failure_class,
        &guardian_decision,
        directive.as_ref(),
    );

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

pub fn guardian_observed_launch_failure_outcome(
    failure_class: LaunchFailureClass,
    crash_evidence: Option<&CrashEvidence>,
    observed_phase: GuardianObservedLaunchFailurePhase,
) -> Option<GuardianUserOutcome> {
    let copy = accepted_launch_failure_copy(failure_class, crash_evidence)?;
    let (decision, phase, summary, detail) = match observed_phase {
        GuardianObservedLaunchFailurePhase::BeforeBoot => (
            GuardianActionKind::Block,
            OperationPhase::Launching,
            "Guardian blocked launch startup.",
            copy.startup_detail.as_str(),
        ),
        GuardianObservedLaunchFailurePhase::AfterBoot => (
            GuardianActionKind::Warn,
            OperationPhase::Running,
            copy.running_summary.as_str(),
            copy.running_detail.as_str(),
        ),
    };
    Some(GuardianUserOutcome {
        decision,
        phase,
        summary: public_summary(summary),
        details: bounded_public_lines([detail]),
        guidance: bounded_public_lines([copy.guidance.as_str()]),
    })
}

struct AcceptedLaunchFailureCopy {
    startup_detail: String,
    running_summary: String,
    running_detail: String,
    guidance: String,
}

fn accepted_launch_failure_copy(
    failure_class: LaunchFailureClass,
    crash_evidence: Option<&CrashEvidence>,
) -> Option<AcceptedLaunchFailureCopy> {
    let copy = match failure_class {
        LaunchFailureClass::OutOfMemory => (
            "Minecraft exited before startup completed after running out of memory.",
            "Minecraft stopped after running out of memory.",
            "Guardian detected an out-of-memory crash after startup completed.",
            "Review the instance memory allocation and close memory-heavy apps before retrying.",
        ),
        LaunchFailureClass::GraphicsDriverCrash => (
            "Minecraft exited before startup completed with a detected graphics driver crash.",
            "Minecraft stopped after a graphics driver crash.",
            "Guardian detected a native graphics driver crash after startup completed.",
            "Update or reinstall the graphics driver, then retry without graphics overlays.",
        ),
        LaunchFailureClass::MissingDependency => (
            "Minecraft exited before startup completed because a required dependency was missing.",
            "Minecraft stopped because a dependency was missing.",
            "Guardian detected a missing class or dependency after startup completed.",
            "Check the installed mods for missing or incompatible dependencies before retrying.",
        ),
        LaunchFailureClass::ModTransformationFailure => (
            "Minecraft exited before startup completed with a detected mod transformation or mixin failure.",
            "Minecraft stopped during mod transformation.",
            "Guardian detected a mod transformation or mixin failure after startup completed.",
            "Update or remove the recently changed mod before retrying.",
        ),
        LaunchFailureClass::ModAttributedCrash => {
            let suspected_mod = suspected_mod_label(crash_evidence);
            let startup_detail = suspected_mod
                .as_ref()
                .map(|name| format!("Minecraft exited before startup completed with a crash attributed to {name}."))
                .unwrap_or_else(|| "Minecraft exited before startup completed with a crash attributed to an installed mod.".to_string());
            let running_summary = suspected_mod
                .as_ref()
                .map(|name| format!("Minecraft stopped in a crash attributed to {name}."))
                .unwrap_or_else(|| "Minecraft stopped in a mod-attributed crash.".to_string());
            let running_detail = suspected_mod
                .as_ref()
                .map(|name| format!("Guardian attributes the crash to the installed mod {name}."))
                .unwrap_or_else(|| {
                    "Guardian found typed crash evidence that attributes the failure to an installed mod."
                        .to_string()
                });
            let guidance = suspected_mod
                .as_ref()
                .map(|name| format!("Update or remove {name} before retrying."))
                .unwrap_or_else(|| {
                    "Update or remove the suspected mod before retrying.".to_string()
                });
            return Some(AcceptedLaunchFailureCopy {
                startup_detail,
                running_summary,
                running_detail,
                guidance,
            });
        }
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

    Some(AcceptedLaunchFailureCopy {
        startup_detail: copy.0.to_string(),
        running_summary: copy.1.to_string(),
        running_detail: copy.2.to_string(),
        guidance: copy.3.to_string(),
    })
}

pub fn conservative_launch_recovery_preset(version_id: &str, runtime_major: u32) -> String {
    if runtime_major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn prepare_failure_diagnosis(request: &GuardianPrepareFailureRequest<'_>) -> Diagnosis {
    match request.failure_class {
        LaunchFailureClass::JavaRuntimeMismatch => Diagnosis {
            id: DiagnosisId::new("java_runtime_major_mismatch"),
            domain: GuardianDomain::Runtime,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: java_override_ownership(request.explicit_java_override_present),
            phase: OperationPhase::Preparing,
            fact_ids: vec![GuardianFactId::JavaMajorMismatch],
            affected_targets: vec![target(
                GuardianDomain::Runtime,
                "explicit_java_override",
                java_override_ownership(request.explicit_java_override_present),
            )],
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: prepare_java_candidate_actions(request),
            public_reason_template: "java_runtime_major_mismatch".to_string(),
        },
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => Diagnosis {
            id: DiagnosisId::new(jvm_failure_diagnosis_id(request.failure_class)),
            domain: GuardianDomain::Jvm,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::Confirmed,
            ownership: jvm_override_ownership(request.explicit_jvm_args_present),
            phase: OperationPhase::Preparing,
            fact_ids: vec![failure_class_fact_id(request.failure_class)],
            affected_targets: vec![target(
                GuardianDomain::Jvm,
                "explicit_jvm_args",
                jvm_override_ownership(request.explicit_jvm_args_present),
            )],
            impact: GuardianImpactVector::launch_blocking(),
            candidate_actions: prepare_jvm_candidate_actions(request),
            public_reason_template: "jvm_arg_unsupported".to_string(),
        },
        failure_class => blocking_launch_diagnosis(
            "launch_prepare_failed",
            OperationPhase::Preparing,
            failure_class,
            "launch_prepare_failed",
        ),
    }
}

fn startup_failure_diagnosis(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
    recovery_template: Option<&StartupRecoveryTemplate>,
) -> Diagnosis {
    match request.observation {
        GuardianStartupFailureObservation::Stalled => Diagnosis {
            id: DiagnosisId::new("startup_stalled"),
            domain: GuardianDomain::Startup,
            severity: GuardianSeverity::Blocking,
            confidence: GuardianConfidence::High,
            ownership: OwnershipClass::LauncherManaged,
            phase: OperationPhase::Launching,
            fact_ids: vec![GuardianFactId::StartupWindowExpired],
            affected_targets: vec![target(
                GuardianDomain::Startup,
                "startup_monitoring",
                OwnershipClass::LauncherManaged,
            )],
            impact: GuardianImpactVector {
                launchability_impact: 0.85,
                ..GuardianImpactVector::default()
            },
            candidate_actions: vec![GuardianActionKind::Block],
            public_reason_template: "startup_stalled".to_string(),
        },
        GuardianStartupFailureObservation::Exited { .. } => match failure_class {
            LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering => Diagnosis {
                id: DiagnosisId::new("jvm_arg_unsupported"),
                domain: GuardianDomain::Jvm,
                severity: GuardianSeverity::Blocking,
                confidence: GuardianConfidence::High,
                ownership: jvm_startup_ownership(request),
                phase: OperationPhase::Launching,
                fact_ids: vec![
                    GuardianFactId::ProcessExitedBeforeBoot,
                    failure_class_fact_id(failure_class),
                ],
                affected_targets: vec![target(
                    GuardianDomain::Jvm,
                    "startup_jvm_settings",
                    jvm_startup_ownership(request),
                )],
                impact: GuardianImpactVector::launch_blocking(),
                candidate_actions: startup_jvm_candidate_actions(recovery_template),
                public_reason_template: "jvm_arg_unsupported".to_string(),
            },
            LaunchFailureClass::JavaRuntimeMismatch => Diagnosis {
                id: DiagnosisId::new("java_runtime_major_mismatch"),
                domain: GuardianDomain::Runtime,
                severity: GuardianSeverity::Blocking,
                confidence: GuardianConfidence::High,
                ownership: java_override_ownership(request.explicit_java_override_present),
                phase: OperationPhase::Launching,
                fact_ids: vec![
                    GuardianFactId::ProcessExitedBeforeBoot,
                    GuardianFactId::JavaMajorMismatch,
                ],
                affected_targets: vec![target(
                    GuardianDomain::Runtime,
                    "startup_java_runtime",
                    java_override_ownership(request.explicit_java_override_present),
                )],
                impact: GuardianImpactVector::launch_blocking(),
                candidate_actions: startup_java_candidate_actions(recovery_template),
                public_reason_template: "java_runtime_major_mismatch".to_string(),
            },
            LaunchFailureClass::OutOfMemory => blocking_user_owned_startup_diagnosis(
                "out_of_memory",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::GraphicsDriverCrash => blocking_user_owned_startup_diagnosis(
                "graphics_driver_crash",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::MissingDependency => blocking_user_owned_startup_diagnosis(
                "missing_dependency",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::ModTransformationFailure => blocking_user_owned_startup_diagnosis(
                "mod_transformation_failure",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::ModAttributedCrash => blocking_user_owned_startup_diagnosis(
                "mod_attributed_crash",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::ClasspathModuleConflict => blocking_startup_diagnosis(
                "classpath_module_conflict",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::LauncherManagedArtifactSignature => Diagnosis {
                id: DiagnosisId::new("launcher_managed_artifact_signature_corrupt"),
                domain: GuardianDomain::Download,
                severity: GuardianSeverity::Blocking,
                confidence: GuardianConfidence::High,
                ownership: OwnershipClass::LauncherManaged,
                phase: OperationPhase::Launching,
                fact_ids: vec![
                    GuardianFactId::ProcessExitedBeforeBoot,
                    failure_class_fact_id(failure_class),
                ],
                affected_targets: vec![target(
                    GuardianDomain::Download,
                    "launcher_managed_jars",
                    OwnershipClass::LauncherManaged,
                )],
                impact: GuardianImpactVector::launch_blocking(),
                candidate_actions: vec![GuardianActionKind::Block],
                public_reason_template: "launcher_managed_artifact_signature_corrupt".to_string(),
            },
            LaunchFailureClass::AuthModeIncompatible => blocking_startup_diagnosis(
                "auth_mode_incompatible",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::LoaderBootstrapFailure => blocking_startup_diagnosis(
                "loader_bootstrap_failure",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::StartupStalled => blocking_startup_diagnosis(
                "startup_stalled",
                failure_class,
                GuardianConfidence::High,
            ),
            LaunchFailureClass::Unknown => blocking_startup_diagnosis(
                "startup_failed_unknown",
                failure_class,
                GuardianConfidence::Low,
            ),
        },
    }
}

fn prepare_failure_directive(
    request: &GuardianPrepareFailureRequest<'_>,
    decision: &GuardianDecision,
) -> Option<GuardianLaunchRecoveryDirective> {
    match (request.failure_class, decision.kind) {
        (LaunchFailureClass::JavaRuntimeMismatch, GuardianActionKind::Fallback)
            if request.requested_java_present
                && request.explicit_java_override_present
                && !request.runtime_intervention_applied =>
        {
            Some(GuardianLaunchRecoveryDirective {
                kind: GuardianLaunchRecoveryKind::SwitchManagedRuntime,
                effect: GuardianLaunchRecoveryEffect::ForceManagedRuntime,
                description: "Guardian switched to managed Java before launch".to_string(),
            })
        }
        (
            LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering,
            GuardianActionKind::Strip,
        ) if request.explicit_jvm_args_present && !request.raw_jvm_args_intervention_applied => {
            Some(GuardianLaunchRecoveryDirective {
                kind: GuardianLaunchRecoveryKind::StripRawJvmArgs,
                effect: GuardianLaunchRecoveryEffect::StripRawJvmArgs,
                description: "Guardian removed incompatible explicit JVM args before launch"
                    .to_string(),
            })
        }
        _ => None,
    }
}

fn startup_failure_directive(
    recovery_template: Option<StartupRecoveryTemplate>,
    decision: &GuardianDecision,
) -> Option<GuardianLaunchRecoveryDirective> {
    let template = recovery_template?;
    let decision_matches = matches!(
        (&template.effect, decision.kind),
        (
            GuardianLaunchRecoveryEffect::DowngradePreset { .. },
            GuardianActionKind::Downgrade
        ) | (
            GuardianLaunchRecoveryEffect::DisableCustomGc,
            GuardianActionKind::Strip
        ) | (
            GuardianLaunchRecoveryEffect::ForceManagedRuntime,
            GuardianActionKind::Fallback
        )
    );
    decision_matches.then_some(GuardianLaunchRecoveryDirective {
        kind: template.kind,
        effect: template.effect,
        description: template.description,
    })
}

fn startup_recovery_template(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
) -> Option<StartupRecoveryTemplate> {
    if request.startup_recovery_applied {
        return None;
    }

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
                    return Some(StartupRecoveryTemplate {
                        kind: GuardianLaunchRecoveryKind::DowngradePreset,
                        effect: GuardianLaunchRecoveryEffect::DowngradePreset {
                            preset: preset.clone(),
                        },
                        description: format!(
                            "Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure"
                        ),
                    });
                }
            }
            (!request.disable_custom_gc).then(|| StartupRecoveryTemplate {
                kind: GuardianLaunchRecoveryKind::DisableCustomGc,
                effect: GuardianLaunchRecoveryEffect::DisableCustomGc,
                description: "Automatic retry: disabled custom GC flags after startup failure"
                    .to_string(),
            })
        }
        LaunchFailureClass::JavaRuntimeMismatch
            if request.requested_java_present && request.explicit_java_override_present =>
        {
            Some(StartupRecoveryTemplate {
                kind: GuardianLaunchRecoveryKind::SwitchManagedRuntime,
                effect: GuardianLaunchRecoveryEffect::ForceManagedRuntime,
                description: "Automatic retry: switched to managed Java after runtime mismatch"
                    .to_string(),
            })
        }
        LaunchFailureClass::OutOfMemory => None,
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct StartupRecoveryTemplate {
    kind: GuardianLaunchRecoveryKind,
    effect: GuardianLaunchRecoveryEffect,
    description: String,
}

fn prepare_failure_user_outcome(
    request: &GuardianPrepareFailureRequest<'_>,
    decision: &GuardianDecision,
    directive: Option<&GuardianLaunchRecoveryDirective>,
) -> GuardianUserOutcome {
    let mut details = Vec::new();
    if let Some(directive) = directive {
        push_public_line(&mut details, &directive.description);
    } else if let Some(detail) = bounded_public_text(request.public_error) {
        push_public_line(&mut details, &detail);
    } else {
        push_public_line(&mut details, prepare_failure_reason(request.failure_class));
    }

    let mut guidance = Vec::new();
    for line in prepare_failure_guidance(
        request.failure_class,
        request.explicit_java_override_present,
        request.explicit_jvm_args_present,
        false,
    ) {
        push_public_line(&mut guidance, line);
    }

    let summary = match decision.kind {
        GuardianActionKind::Fallback | GuardianActionKind::Strip => {
            "Guardian adjusted launch preparation."
        }
        GuardianActionKind::AskUser => "Guardian needs confirmation before launch preparation.",
        GuardianActionKind::Block => "Guardian blocked launch preparation.",
        _ => "Guardian recorded launch preparation failure.",
    };

    GuardianUserOutcome {
        decision: public_user_decision(decision.kind),
        phase: OperationPhase::Preparing,
        summary: public_summary(summary),
        details: capped_lines(details),
        guidance: capped_lines(guidance),
    }
}

fn startup_failure_user_outcome(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
    decision: &GuardianDecision,
    directive: Option<&GuardianLaunchRecoveryDirective>,
) -> GuardianUserOutcome {
    let mut details = Vec::new();
    if let Some(directive) = directive {
        push_public_line(&mut details, &directive.description);
    } else {
        push_public_line(
            &mut details,
            &startup_failure_reason(request, failure_class),
        );
    }

    let mut guidance = Vec::new();
    for line in startup_failure_guidance(request, failure_class) {
        push_public_line(&mut guidance, &line);
    }

    let summary = match decision.kind {
        GuardianActionKind::Downgrade
        | GuardianActionKind::Strip
        | GuardianActionKind::Fallback => "Guardian selected a guarded startup retry.",
        GuardianActionKind::AskUser => "Guardian needs confirmation before startup recovery.",
        GuardianActionKind::Block => "Guardian blocked launch startup.",
        _ => "Guardian recorded launch startup failure.",
    };

    GuardianUserOutcome {
        decision: public_user_decision(decision.kind),
        phase: OperationPhase::Launching,
        summary: public_summary(summary),
        details: capped_lines(details),
        guidance: capped_lines(guidance),
    }
}

fn public_user_decision(decision: GuardianActionKind) -> GuardianActionKind {
    match decision {
        GuardianActionKind::Fallback
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Retry => decision,
        GuardianActionKind::AskUser => GuardianActionKind::AskUser,
        GuardianActionKind::Block => GuardianActionKind::Block,
        GuardianActionKind::Allow | GuardianActionKind::RecordOnly => decision,
        _ => GuardianActionKind::Warn,
    }
}

fn prepare_java_candidate_actions(
    request: &GuardianPrepareFailureRequest<'_>,
) -> Vec<GuardianActionKind> {
    if request.requested_java_present
        && request.explicit_java_override_present
        && !request.runtime_intervention_applied
    {
        vec![
            GuardianActionKind::Fallback,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ]
    } else {
        vec![GuardianActionKind::Block]
    }
}

fn prepare_jvm_candidate_actions(
    request: &GuardianPrepareFailureRequest<'_>,
) -> Vec<GuardianActionKind> {
    if request.explicit_jvm_args_present && !request.raw_jvm_args_intervention_applied {
        vec![
            GuardianActionKind::Strip,
            GuardianActionKind::AskUser,
            GuardianActionKind::Block,
        ]
    } else {
        vec![GuardianActionKind::Block]
    }
}

fn startup_jvm_candidate_actions(
    recovery_template: Option<&StartupRecoveryTemplate>,
) -> Vec<GuardianActionKind> {
    match recovery_template.map(|template| &template.effect) {
        Some(GuardianLaunchRecoveryEffect::DowngradePreset { .. }) => {
            vec![GuardianActionKind::Downgrade, GuardianActionKind::Block]
        }
        Some(GuardianLaunchRecoveryEffect::DisableCustomGc) => {
            vec![GuardianActionKind::Strip, GuardianActionKind::Block]
        }
        _ => vec![GuardianActionKind::Block],
    }
}

fn startup_java_candidate_actions(
    recovery_template: Option<&StartupRecoveryTemplate>,
) -> Vec<GuardianActionKind> {
    if matches!(
        recovery_template.map(|template| &template.effect),
        Some(GuardianLaunchRecoveryEffect::ForceManagedRuntime)
    ) {
        vec![GuardianActionKind::Fallback, GuardianActionKind::Block]
    } else {
        vec![GuardianActionKind::Block]
    }
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

fn safety_case(mode: GuardianMode, phase: OperationPhase, diagnosis: Diagnosis) -> SafetyCase {
    SafetyCase {
        operation_id: None,
        mode,
        phase,
        diagnoses: vec![diagnosis],
    }
}

fn blocking_launch_diagnosis(
    id: &str,
    phase: OperationPhase,
    failure_class: LaunchFailureClass,
    target_id: &str,
) -> Diagnosis {
    Diagnosis {
        id: DiagnosisId::new(id),
        domain: GuardianDomain::Launch,
        severity: GuardianSeverity::Blocking,
        confidence: if failure_class == LaunchFailureClass::Unknown {
            GuardianConfidence::Low
        } else {
            GuardianConfidence::High
        },
        ownership: OwnershipClass::LauncherManaged,
        phase,
        fact_ids: vec![failure_class_fact_id(failure_class)],
        affected_targets: vec![target(
            GuardianDomain::Launch,
            target_id,
            OwnershipClass::LauncherManaged,
        )],
        impact: GuardianImpactVector::launch_blocking(),
        candidate_actions: vec![GuardianActionKind::Block],
        public_reason_template: id.to_string(),
    }
}

fn blocking_startup_diagnosis(
    id: &str,
    failure_class: LaunchFailureClass,
    confidence: GuardianConfidence,
) -> Diagnosis {
    blocking_startup_diagnosis_with_ownership(
        id,
        failure_class,
        confidence,
        OwnershipClass::LauncherManaged,
        "startup_monitoring",
    )
}

fn blocking_user_owned_startup_diagnosis(
    id: &str,
    failure_class: LaunchFailureClass,
    confidence: GuardianConfidence,
) -> Diagnosis {
    blocking_startup_diagnosis_with_ownership(
        id,
        failure_class,
        confidence,
        OwnershipClass::UserOwned,
        "instance_crash",
    )
}

fn blocking_startup_diagnosis_with_ownership(
    id: &str,
    failure_class: LaunchFailureClass,
    confidence: GuardianConfidence,
    ownership: OwnershipClass,
    target_id: &str,
) -> Diagnosis {
    Diagnosis {
        id: DiagnosisId::new(id),
        domain: GuardianDomain::Startup,
        severity: GuardianSeverity::Blocking,
        confidence,
        ownership,
        phase: OperationPhase::Launching,
        fact_ids: vec![
            GuardianFactId::ProcessExitedBeforeBoot,
            failure_class_fact_id(failure_class),
        ],
        affected_targets: vec![target(GuardianDomain::Startup, target_id, ownership)],
        impact: GuardianImpactVector::launch_blocking(),
        candidate_actions: vec![GuardianActionKind::Block],
        public_reason_template: id.to_string(),
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

fn jvm_failure_diagnosis_id(failure_class: LaunchFailureClass) -> &'static str {
    match failure_class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => "jvm_arg_unsupported",
        _ => "jvm_failure",
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

fn prepare_failure_reason(failure_class: LaunchFailureClass) -> &'static str {
    match failure_class {
        LaunchFailureClass::JavaRuntimeMismatch => {
            "The selected Java runtime is not compatible with this version."
        }
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            "The selected JVM settings are not compatible with this Java runtime."
        }
        _ => "Launch preparation failed before Minecraft could start.",
    }
}

fn startup_failure_reason(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
) -> String {
    if let Some(copy) = accepted_launch_failure_copy(failure_class, request.crash_evidence) {
        return copy.startup_detail;
    }
    let reason = match request.observation {
        GuardianStartupFailureObservation::Stalled => {
            "No startup activity was observed before the startup window ended."
        }
        GuardianStartupFailureObservation::Exited { .. } => match failure_class {
            LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering => {
                "Minecraft exited before startup completed with a detected JVM option compatibility failure."
            }
            LaunchFailureClass::JavaRuntimeMismatch => {
                "Minecraft exited before startup completed with a detected Java runtime mismatch."
            }
            LaunchFailureClass::ClasspathModuleConflict => {
                "Minecraft exited before startup completed with a detected classpath or module conflict."
            }
            LaunchFailureClass::LauncherManagedArtifactSignature => {
                "Minecraft exited before startup completed with detected launcher-managed jar signature corruption."
            }
            LaunchFailureClass::AuthModeIncompatible => {
                "Minecraft exited before startup completed because the selected auth mode was not launch-ready."
            }
            LaunchFailureClass::LoaderBootstrapFailure => {
                "Minecraft exited before startup completed with a detected loader bootstrap failure."
            }
            LaunchFailureClass::StartupStalled => {
                "Minecraft exited before startup completed after startup activity stalled."
            }
            LaunchFailureClass::OutOfMemory
            | LaunchFailureClass::GraphicsDriverCrash
            | LaunchFailureClass::MissingDependency
            | LaunchFailureClass::ModTransformationFailure
            | LaunchFailureClass::ModAttributedCrash
            | LaunchFailureClass::Unknown => {
                "Minecraft exited before Guardian could verify a completed startup."
            }
        },
    };
    reason.to_string()
}

fn prepare_failure_guidance(
    failure_class: LaunchFailureClass,
    explicit_java_override_present: bool,
    explicit_jvm_args_present: bool,
    explicit_jvm_preset_present: bool,
) -> Vec<&'static str> {
    match failure_class {
        LaunchFailureClass::JavaRuntimeMismatch => {
            if explicit_java_override_present {
                vec!["Remove the Java override or switch Guardian Mode back to Managed."]
            } else {
                vec!["Use a compatible Java runtime or let Axial use the managed runtime."]
            }
        }
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if explicit_jvm_args_present {
                vec!["Remove the explicit JVM args or switch Guardian Mode back to Managed."]
            } else if explicit_jvm_preset_present {
                vec!["Choose a safer JVM preset or switch Guardian Mode back to Managed."]
            } else {
                vec!["Use safer launch settings or let Axial manage compatibility."]
            }
        }
        LaunchFailureClass::StartupStalled => {
            vec!["Launch stalled before startup. Review recent override changes first."]
        }
        LaunchFailureClass::LauncherManagedArtifactSignature => {
            vec![
                "Repair the installed version so Axial can replace the affected launcher-managed jars.",
            ]
        }
        _ => Vec::new(),
    }
}

fn startup_failure_guidance(
    request: &GuardianStartupFailureRequest<'_>,
    failure_class: LaunchFailureClass,
) -> Vec<String> {
    if let Some(copy) = accepted_launch_failure_copy(failure_class, request.crash_evidence) {
        return vec![copy.guidance];
    }
    if failure_class == LaunchFailureClass::StartupStalled {
        return if request_has_explicit_startup_intent(request) {
            vec![
                "Review recent Java, JVM preset, or JVM argument overrides before retrying."
                    .to_string(),
            ]
        } else {
            vec!["Review the latest game log before retrying.".to_string()]
        };
    }

    let mut guidance = prepare_failure_guidance(
        failure_class,
        request.explicit_java_override_present,
        request.explicit_jvm_args_present,
        request.explicit_jvm_preset_present,
    )
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    if !guidance.is_empty() {
        return guidance;
    }
    if request_has_explicit_startup_intent(request) {
        guidance.push(
            "Review recent Java, JVM preset, or JVM argument overrides before retrying."
                .to_string(),
        );
    } else {
        guidance.push("Review the latest game log before retrying.".to_string());
    }
    guidance
}

fn bounded_public_text(value: &str) -> Option<String> {
    sanitize_evidence_text(
        value,
        RedactionAudience::UserVisible,
        MAX_LAUNCH_DECISION_DETAIL_CHARS,
    )
}

fn suspected_mod_label(crash_evidence: Option<&CrashEvidence>) -> Option<String> {
    crash_evidence
        .and_then(|evidence| evidence.suspected_mods.first())
        .and_then(|suspected_mod| bounded_public_text(suspected_mod.name.as_str()))
}

fn bounded_public_lines<const N: usize>(values: [&str; N]) -> Vec<String> {
    values
        .into_iter()
        .filter_map(bounded_public_text)
        .take(MAX_LAUNCH_DECISION_LINES)
        .collect()
}

fn public_summary(value: &str) -> String {
    sanitize_evidence_text(
        value,
        RedactionAudience::UserVisible,
        MAX_LAUNCH_DECISION_SUMMARY_CHARS,
    )
    .unwrap_or_else(|| "Guardian recorded launch safety outcome.".to_string())
}

fn safe_preset_label(value: &str) -> String {
    let sanitized = value
        .chars()
        .take(64)
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .collect::<String>();
    if sanitized.trim().is_empty() {
        "none".to_string()
    } else {
        sanitized
    }
}

fn push_public_line(lines: &mut Vec<String>, value: &str) {
    if lines.len() >= MAX_LAUNCH_DECISION_LINES {
        return;
    }
    let Some(value) = sanitize_evidence_text(
        value,
        RedactionAudience::UserVisible,
        MAX_LAUNCH_DECISION_DETAIL_CHARS,
    ) else {
        return;
    };
    let value = value.trim();
    if value.is_empty() || lines.iter().any(|line| line == value) {
        return;
    }
    lines.push(value.to_string());
}

fn capped_lines(mut lines: Vec<String>) -> Vec<String> {
    lines.truncate(MAX_LAUNCH_DECISION_LINES);
    lines
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
        GuardianLaunchRecoveryEffect, GuardianObservedLaunchFailurePhase,
        GuardianPrepareFailureRequest, GuardianStartupFailureObservation,
        GuardianStartupFailureRequest, guardian_observed_launch_failure_outcome,
        guardian_prelaunch_preset_adjustment_directive, guardian_prepare_failure_outcome,
        guardian_startup_failure_outcome,
    };
    use crate::guardian::{
        GuardianActionKind, GuardianFactId, GuardianLaunchRecoveryKind, GuardianMode,
        conservative_launch_recovery_preset,
    };
    use crate::state::contracts::{OperationPhase, OwnershipClass};
    use axial_launcher::{CrashEvidence, LaunchFailureClass};

    #[test]
    fn managed_prepare_java_mismatch_returns_managed_runtime_fallback_directive() {
        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: GuardianMode::Managed,
            failure_class: LaunchFailureClass::JavaRuntimeMismatch,
            public_error: "Java 8 cannot launch this version.",
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: false,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });

        let directive = outcome.directive.expect("fallback directive");
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Fallback);
        assert_eq!(
            directive.kind,
            GuardianLaunchRecoveryKind::SwitchManagedRuntime
        );
        assert_eq!(
            directive.effect,
            GuardianLaunchRecoveryEffect::ForceManagedRuntime
        );
    }

    #[test]
    fn managed_prepare_jvm_unsupported_option_returns_strip_directive() {
        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: GuardianMode::Managed,
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            public_error: "Unsupported VM option.",
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: true,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });

        let directive = outcome.directive.expect("strip directive");
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Strip);
        assert_eq!(directive.kind, GuardianLaunchRecoveryKind::StripRawJvmArgs);
        assert_eq!(
            directive.effect,
            GuardianLaunchRecoveryEffect::StripRawJvmArgs
        );
    }

    #[test]
    fn custom_explicit_override_does_not_return_silent_mutation_directive() {
        let outcome = guardian_prepare_failure_outcome(GuardianPrepareFailureRequest {
            mode: GuardianMode::Custom,
            failure_class: LaunchFailureClass::JvmUnsupportedOption,
            public_error: "Unsupported VM option.",
            requested_java_present: false,
            explicit_java_override_present: false,
            explicit_jvm_args_present: true,
            runtime_intervention_applied: false,
            raw_jvm_args_intervention_applied: false,
        });

        assert!(outcome.directive.is_none());
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::AskUser);
    }

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

        assert_eq!(directive.kind, GuardianLaunchRecoveryKind::DowngradePreset);
        assert_eq!(
            directive.effect,
            GuardianLaunchRecoveryEffect::DowngradePreset {
                preset: "performance".to_string()
            }
        );
        assert_eq!(
            directive.description,
            "Guardian downgraded JVM preset from \"ultra_low_latency\" to \"performance\" before launch"
        );
    }

    #[test]
    fn custom_prelaunch_preset_adjustment_does_not_return_silent_directive() {
        let directive = guardian_prelaunch_preset_adjustment_directive(
            super::GuardianPresetAdjustmentRequest {
                mode: GuardianMode::Custom,
                requested_preset: "ultra_low_latency",
                effective_preset: "performance",
                explicit_jvm_preset_present: true,
            },
        );

        assert!(directive.is_none());
    }

    #[test]
    fn startup_jvm_unsupported_option_returns_downgrade_when_conservative_preset_differs() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
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
            effective_preset: "ultra_low_latency",
        });

        let directive = outcome.directive.expect("downgrade directive");
        assert_eq!(
            outcome.guardian_decision.kind,
            GuardianActionKind::Downgrade
        );
        assert_eq!(directive.kind, GuardianLaunchRecoveryKind::DowngradePreset);
        assert_eq!(
            directive.effect,
            GuardianLaunchRecoveryEffect::DowngradePreset {
                preset: "performance".to_string()
            }
        );
    }

    #[test]
    fn startup_jvm_unsupported_option_returns_disable_custom_gc_when_no_downgrade_exists() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JvmUnsupportedOption,
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

        let directive = outcome.directive.expect("disable gc directive");
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Strip);
        assert_eq!(directive.kind, GuardianLaunchRecoveryKind::DisableCustomGc);
        assert_eq!(
            directive.effect,
            GuardianLaunchRecoveryEffect::DisableCustomGc
        );
    }

    #[test]
    fn startup_java_runtime_mismatch_returns_managed_runtime_switch_in_managed_mode() {
        let outcome = guardian_startup_failure_outcome(GuardianStartupFailureRequest {
            mode: GuardianMode::Managed,
            observation: GuardianStartupFailureObservation::Exited {
                failure_class: LaunchFailureClass::JavaRuntimeMismatch,
            },
            crash_evidence: None,
            target_version_id: "1.21.1",
            runtime_major: 8,
            requested_java_present: true,
            explicit_java_override_present: true,
            explicit_jvm_args_present: false,
            explicit_jvm_preset_present: false,
            startup_recovery_applied: false,
            disable_custom_gc: false,
            effective_preset: "performance",
        });

        let directive = outcome.directive.expect("runtime switch directive");
        assert_eq!(outcome.guardian_decision.kind, GuardianActionKind::Fallback);
        assert_eq!(
            directive.kind,
            GuardianLaunchRecoveryKind::SwitchManagedRuntime
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
            outcome.safety_case.diagnoses[0].id.as_str(),
            "launcher_managed_artifact_signature_corrupt"
        );
        assert!(outcome.directive.is_none());
        assert!(outcome.user_outcome.details.contains(
            &"Minecraft exited before startup completed with detected launcher-managed jar signature corruption.".to_string()
        ));
        assert!(outcome.user_outcome.guidance.contains(
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
        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Block);
        assert_eq!(
            outcome.safety_case.diagnoses[0].id.as_str(),
            "out_of_memory"
        );
        assert!(
            outcome.safety_case.diagnoses[0]
                .fact_ids
                .contains(&GuardianFactId::OutOfMemory)
        );
        assert!(outcome.directive.is_none());
        assert_eq!(
            outcome.user_outcome.summary,
            "Guardian blocked launch startup."
        );
        assert_eq!(
            outcome.user_outcome.details,
            ["Minecraft exited before startup completed after running out of memory."]
        );
        assert_eq!(
            outcome.user_outcome.guidance,
            ["Review the instance memory allocation and close memory-heavy apps before retrying."]
        );
        assert!(
            outcome.user_outcome.summary.chars().count()
                <= super::MAX_LAUNCH_DECISION_SUMMARY_CHARS
        );
        assert!(
            outcome
                .user_outcome
                .details
                .iter()
                .chain(&outcome.user_outcome.guidance)
                .all(|line| line.chars().count() <= super::MAX_LAUNCH_DECISION_DETAIL_CHARS)
        );

        assert_eq!(
            outcome.safety_case.diagnoses[0].ownership,
            OwnershipClass::UserOwned
        );

        let post_boot = guardian_observed_launch_failure_outcome(
            LaunchFailureClass::OutOfMemory,
            None,
            GuardianObservedLaunchFailurePhase::AfterBoot,
        )
        .expect("OOM is an accepted launch crash");
        assert_eq!(post_boot.decision, GuardianActionKind::Warn);
        assert_eq!(post_boot.phase, OperationPhase::Running);
        assert_eq!(
            post_boot.summary,
            "Minecraft stopped after running out of memory."
        );
        assert!(post_boot.summary.chars().count() <= super::MAX_LAUNCH_DECISION_SUMMARY_CHARS);
        assert!(
            post_boot
                .details
                .iter()
                .chain(&post_boot.guidance)
                .all(|line| line.chars().count() <= super::MAX_LAUNCH_DECISION_DETAIL_CHARS)
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
                outcome.safety_case.diagnoses[0].id.as_str(),
                failure_class.as_str()
            );
            assert_eq!(
                outcome.safety_case.diagnoses[0].ownership,
                OwnershipClass::UserOwned
            );
            assert!(outcome.directive.is_none());
            assert!(!outcome.user_outcome.details.is_empty());
            assert!(!outcome.user_outcome.guidance.is_empty());
            assert!(
                outcome
                    .user_outcome
                    .details
                    .iter()
                    .chain(&outcome.user_outcome.guidance)
                    .all(|line| line.chars().count() <= super::MAX_LAUNCH_DECISION_DETAIL_CHARS)
            );

            let before_boot = guardian_observed_launch_failure_outcome(
                failure_class,
                None,
                GuardianObservedLaunchFailurePhase::BeforeBoot,
            )
            .expect("accepted before-boot crash outcome");
            assert_eq!(before_boot.decision, GuardianActionKind::Block);
            assert_eq!(before_boot.phase, OperationPhase::Launching);
            assert_eq!(before_boot.details, outcome.user_outcome.details);
            assert_eq!(before_boot.guidance, outcome.user_outcome.guidance);

            let post_boot = guardian_observed_launch_failure_outcome(
                failure_class,
                None,
                GuardianObservedLaunchFailurePhase::AfterBoot,
            )
            .expect("accepted post-boot crash outcome");
            assert_eq!(post_boot.phase, OperationPhase::Running);
            assert!(!post_boot.details.is_empty());
            assert!(!post_boot.guidance.is_empty());
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
        let post_boot = guardian_observed_launch_failure_outcome(
            LaunchFailureClass::ModAttributedCrash,
            Some(&crash_evidence),
            GuardianObservedLaunchFailurePhase::AfterBoot,
        )
        .expect("mod-attributed post-boot outcome");

        assert!(startup.user_outcome.details[0].contains("Example Machines"));
        assert!(startup.user_outcome.guidance[0].contains("Example Machines"));
        assert!(post_boot.summary.contains("Example Machines"));
        assert!(post_boot.details[0].contains("Example Machines"));
        assert!(post_boot.guidance[0].contains("Example Machines"));
        assert!(
            guardian_observed_launch_failure_outcome(
                LaunchFailureClass::Unknown,
                None,
                GuardianObservedLaunchFailurePhase::AfterBoot,
            )
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
        assert_eq!(outcome.user_outcome.decision, GuardianActionKind::Block);
        assert_eq!(
            outcome.user_outcome.summary,
            "Guardian blocked launch startup."
        );
        assert!(outcome.directive.is_none());
        assert!(outcome.user_outcome.details.contains(
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
            outcome.user_outcome.summary,
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
