use crate::types::LaunchFailureClass;
use croopor_minecraft::JavaRuntimeInfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardianMode {
    #[default]
    Managed,
    Custom,
}

impl GuardianMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Custom => "custom",
        }
    }

    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            "custom" => Self::Custom,
            _ => Self::Managed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideOrigin {
    Global,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LaunchGuardianContext {
    pub mode: GuardianMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_override_origin: Option<OverrideOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_override_origin: Option<OverrideOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_jvm_args_origin: Option<OverrideOrigin>,
}

impl LaunchGuardianContext {
    pub fn has_java_override(&self) -> bool {
        self.java_override_origin.is_some()
    }

    pub fn has_named_preset(&self) -> bool {
        self.preset_override_origin.is_some()
    }

    pub fn has_raw_jvm_args(&self) -> bool {
        self.raw_jvm_args_origin.is_some()
    }

    pub fn has_risky_overrides(&self) -> bool {
        self.has_java_override() || self.has_named_preset() || self.has_raw_jvm_args()
    }

    pub fn allows_runtime_healing(&self) -> bool {
        matches!(self.mode, GuardianMode::Managed) && self.has_java_override()
    }

    pub fn allows_preset_healing(&self) -> bool {
        matches!(self.mode, GuardianMode::Managed) || !self.has_named_preset()
    }

    pub fn allows_raw_jvm_arg_intervention(&self) -> bool {
        matches!(self.mode, GuardianMode::Managed) && self.has_raw_jvm_args()
    }

    pub fn allows_prelaunch_preset_intervention(&self) -> bool {
        matches!(self.mode, GuardianMode::Managed) && self.has_named_preset()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianDecision {
    Allowed,
    Warned,
    Blocked,
    Intervened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardianInterventionKind {
    SwitchManagedRuntime,
    StripJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardianIntervention {
    pub kind: GuardianInterventionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardianSummary {
    pub mode: GuardianMode,
    pub decision: GuardianDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interventions: Vec<GuardianIntervention>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreLaunchAction {
    ForceManagedRuntime,
    StripRawJvmArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreLaunchDecision {
    Allow,
    Intervene {
        action: PreLaunchAction,
        kind: GuardianInterventionKind,
        description: String,
    },
    Block {
        class: LaunchFailureClass,
        message: String,
        guidance: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct RecoveryPlan {
    pub description: String,
    pub action: RecoveryAction,
}

#[derive(Debug, Clone)]
pub enum RecoveryAction {
    DowngradePreset(String),
    DisableCustomGc,
    SwitchManagedRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGuardianPreset {
    pub effective_preset: String,
    pub intervention: Option<GuardianIntervention>,
}

impl GuardianSummary {
    pub fn new(mode: GuardianMode) -> Self {
        Self {
            mode,
            decision: GuardianDecision::Allowed,
            message: None,
            details: Vec::new(),
            guidance: Vec::new(),
            interventions: Vec::new(),
        }
    }

    pub fn record_intervention(
        &mut self,
        kind: GuardianInterventionKind,
        detail: impl Into<String>,
        silent: bool,
    ) {
        self.decision = GuardianDecision::Intervened;
        self.interventions.push(GuardianIntervention {
            kind,
            detail: Some(detail.into()),
            silent: Some(silent),
        });
        self.refresh_outcome();
    }

    pub fn block_with_guidance(&mut self, guidance: Vec<String>) {
        self.decision = GuardianDecision::Blocked;
        self.guidance = guidance;
        self.refresh_outcome();
    }

    pub fn warn_with_guidance(&mut self, guidance: Vec<String>) {
        self.decision = GuardianDecision::Warned;
        for detail in guidance {
            push_unique_detail(&mut self.guidance, Some(detail));
        }
        self.refresh_outcome();
    }

    fn refresh_outcome(&mut self) {
        self.message = guardian_message(self.decision).map(str::to_string);
        self.details = guardian_details(self.decision, &self.interventions, &self.guidance);
    }
}

fn guardian_message(decision: GuardianDecision) -> Option<&'static str> {
    match decision {
        GuardianDecision::Allowed => None,
        GuardianDecision::Warned => Some("Guardian flagged launch settings for review."),
        GuardianDecision::Blocked => Some("Guardian blocked an unsafe launch setup."),
        GuardianDecision::Intervened => Some("Guardian adjusted launch settings for safety."),
    }
}

fn guardian_details(
    decision: GuardianDecision,
    interventions: &[GuardianIntervention],
    guidance: &[String],
) -> Vec<String> {
    let mut details = Vec::new();
    if matches!(
        decision,
        GuardianDecision::Intervened | GuardianDecision::Blocked | GuardianDecision::Warned
    ) {
        for intervention in interventions {
            if intervention.silent.unwrap_or(false) {
                continue;
            }
            push_unique_detail(&mut details, user_facing_intervention_detail(intervention));
        }
    }
    if matches!(
        decision,
        GuardianDecision::Blocked | GuardianDecision::Warned
    ) {
        for detail in guidance {
            push_unique_detail(&mut details, Some(detail.clone()));
        }
    }
    details
}

fn push_unique_detail(details: &mut Vec<String>, detail: Option<String>) {
    let Some(detail) = detail else {
        return;
    };
    let detail = detail.trim();
    if detail.is_empty() || details.iter().any(|existing| existing == detail) {
        return;
    }
    details.push(detail.to_string());
}

fn user_facing_intervention_detail(intervention: &GuardianIntervention) -> Option<String> {
    match intervention.kind {
        GuardianInterventionKind::SwitchManagedRuntime => {
            Some("Guardian used the managed Java runtime for this launch.".to_string())
        }
        GuardianInterventionKind::StripJvmArgs => Some(
            "Explicit JVM args were removed before launch because they were incompatible."
                .to_string(),
        ),
        GuardianInterventionKind::DowngradePreset => Some(downgrade_preset_detail(
            intervention.detail.as_deref().unwrap_or_default(),
        )),
        GuardianInterventionKind::DisableCustomGc => {
            Some("Custom GC flags were disabled for compatibility.".to_string())
        }
    }
}

fn downgrade_preset_detail(detail: &str) -> String {
    let quoted = detail
        .split('"')
        .skip(1)
        .step_by(2)
        .map(format_preset_name)
        .collect::<Vec<_>>();
    match quoted.as_slice() {
        [from, to, ..] => format!("JVM preset changed from {from} to {to} for compatibility."),
        [to] => format!("JVM preset changed to {to} for compatibility."),
        [] => "JVM preset was changed for compatibility.".to_string(),
    }
}

fn format_preset_name(preset: &str) -> String {
    match preset {
        "" | "none" => "Auto".to_string(),
        "smooth" => "Smooth".to_string(),
        "performance" => "Performance".to_string(),
        "ultra_low_latency" => "Ultra Low Latency".to_string(),
        "graalvm" => "GraalVM".to_string(),
        "legacy" => "Legacy".to_string(),
        "legacy_pvp" => "Legacy PvP".to_string(),
        "legacy_heavy" => "Legacy Heavy".to_string(),
        value => value
            .split('_')
            .filter(|part| !part.is_empty())
            .map(capitalize_ascii_word)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn capitalize_ascii_word(word: &str) -> String {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result = String::new();
    result.push(first.to_ascii_uppercase());
    result.extend(chars);
    result
}

pub fn guidance_for_failure(
    class: LaunchFailureClass,
    context: &LaunchGuardianContext,
) -> Vec<String> {
    match class {
        LaunchFailureClass::JavaRuntimeMismatch => {
            if context.has_java_override() {
                vec![
                    "Remove the Java override or switch Guardian Mode back to Managed.".to_string(),
                ]
            } else {
                vec![
                    "Use a compatible Java runtime or let Croopor use the managed runtime."
                        .to_string(),
                ]
            }
        }
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if context.has_raw_jvm_args() {
                vec![
                    "Remove the explicit JVM args or switch Guardian Mode back to Managed."
                        .to_string(),
                ]
            } else if context.has_named_preset() {
                vec![
                    "Choose a safer JVM preset or switch Guardian Mode back to Managed."
                        .to_string(),
                ]
            } else {
                vec!["Use safer launch settings or let Croopor manage compatibility.".to_string()]
            }
        }
        LaunchFailureClass::StartupStalled => {
            vec!["Launch stalled before startup. Review recent override changes first.".to_string()]
        }
        _ => Vec::new(),
    }
}

pub fn decide_prepare_failure(
    context: &LaunchGuardianContext,
    failure_class: LaunchFailureClass,
    message: &str,
    requested_java: &str,
    extra_jvm_args: &[String],
    runtime_intervention_applied: bool,
    raw_jvm_args_intervention_applied: bool,
) -> PreLaunchDecision {
    if failure_class == LaunchFailureClass::JavaRuntimeMismatch
        && !runtime_intervention_applied
        && !requested_java.trim().is_empty()
        && context.allows_runtime_healing()
    {
        return PreLaunchDecision::Intervene {
            action: PreLaunchAction::ForceManagedRuntime,
            kind: GuardianInterventionKind::SwitchManagedRuntime,
            description: "Guardian switched to managed Java before launch".to_string(),
        };
    }

    if matches!(
        failure_class,
        LaunchFailureClass::JvmUnsupportedOption
            | LaunchFailureClass::JvmExperimentalUnlock
            | LaunchFailureClass::JvmOptionOrdering
    ) && !raw_jvm_args_intervention_applied
        && !extra_jvm_args.is_empty()
        && context.allows_raw_jvm_arg_intervention()
    {
        return PreLaunchDecision::Intervene {
            action: PreLaunchAction::StripRawJvmArgs,
            kind: GuardianInterventionKind::StripJvmArgs,
            description: "Guardian removed incompatible explicit JVM args before launch"
                .to_string(),
        };
    }

    PreLaunchDecision::Block {
        class: failure_class,
        message: message.to_string(),
        guidance: guidance_for_failure(failure_class, context),
    }
}

pub fn resolve_launch_preset(
    context: &LaunchGuardianContext,
    requested_preset: &str,
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> Result<ResolvedGuardianPreset, (LaunchFailureClass, String)> {
    let requested = requested_preset.trim();
    let effective = crate::jvm::recommended_preset(requested, version_id, loader, is_modded, info);
    if requested.is_empty() || requested == effective {
        return Ok(ResolvedGuardianPreset {
            effective_preset: effective,
            intervention: None,
        });
    }

    if context.allows_prelaunch_preset_intervention() {
        let detail = format!(
            "Guardian downgraded JVM preset from \"{requested}\" to \"{effective}\" before launch"
        );
        return Ok(ResolvedGuardianPreset {
            effective_preset: effective,
            intervention: Some(GuardianIntervention {
                kind: GuardianInterventionKind::DowngradePreset,
                detail: Some(detail),
                silent: Some(false),
            }),
        });
    }

    Ok(ResolvedGuardianPreset {
        effective_preset: requested.to_string(),
        intervention: None,
    })
}

pub fn recovery_plan_for_startup_failure(
    class: LaunchFailureClass,
    version_id: &str,
    info: &JavaRuntimeInfo,
    requested_java: &str,
    guardian: &LaunchGuardianContext,
    disable_custom_gc: bool,
    effective_preset: &str,
) -> Option<RecoveryPlan> {
    match class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if !guardian.allows_preset_healing() {
                return None;
            }
            if !effective_preset.trim().is_empty() {
                let preset = conservative_healing_preset(version_id, info);
                if !preset.is_empty() && preset != effective_preset {
                    return Some(RecoveryPlan {
                        description: format!(
                            "Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure"
                        ),
                        action: RecoveryAction::DowngradePreset(preset),
                    });
                }
            }
            if !disable_custom_gc {
                return Some(RecoveryPlan {
                    description: "Automatic retry: disabled custom GC flags after startup failure"
                        .to_string(),
                    action: RecoveryAction::DisableCustomGc,
                });
            }
        }
        LaunchFailureClass::JavaRuntimeMismatch
            if !requested_java.trim().is_empty() && guardian.allows_runtime_healing() =>
        {
            return Some(RecoveryPlan {
                description: "Automatic retry: switched to managed Java after runtime mismatch"
                    .to_string(),
                action: RecoveryAction::SwitchManagedRuntime,
            });
        }
        _ => {}
    }
    None
}

pub fn conservative_healing_preset(version_id: &str, info: &JavaRuntimeInfo) -> String {
    if info.major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn is_legacy_version_family(version_id: &str) -> bool {
    let base = version_id.split("-forge-").next().unwrap_or(version_id);
    if matches!(base.as_bytes().first(), Some(b'a' | b'b')) {
        return true;
    }
    let numbers = base
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 12)
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianInterventionKind, GuardianMode, GuardianSummary, LaunchGuardianContext,
        OverrideOrigin, PreLaunchAction, PreLaunchDecision, RecoveryAction,
        conservative_healing_preset, decide_prepare_failure, recovery_plan_for_startup_failure,
    };
    use crate::types::LaunchFailureClass;
    use croopor_minecraft::JavaRuntimeInfo;
    use serde_json::json;

    #[test]
    fn custom_mode_keeps_explicit_preset_out_of_automatic_healing() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Custom,
            java_override_origin: None,
            preset_override_origin: Some(OverrideOrigin::Instance),
            raw_jvm_args_origin: None,
        };

        assert!(!context.allows_preset_healing());
    }

    #[test]
    fn named_preset_counts_as_risky_override_for_warning_policy() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Custom,
            java_override_origin: None,
            preset_override_origin: Some(OverrideOrigin::Instance),
            raw_jvm_args_origin: None,
        };

        assert!(context.has_risky_overrides());
    }

    #[test]
    fn managed_mode_allows_raw_jvm_arg_intervention() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Managed,
            java_override_origin: None,
            preset_override_origin: None,
            raw_jvm_args_origin: Some(OverrideOrigin::Instance),
        };

        assert!(context.allows_raw_jvm_arg_intervention());
    }

    #[test]
    fn managed_mode_intervenes_on_prepare_failure_for_manual_java() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Managed,
            java_override_origin: Some(OverrideOrigin::Instance),
            preset_override_origin: None,
            raw_jvm_args_origin: None,
        };

        let decision = decide_prepare_failure(
            &context,
            LaunchFailureClass::JavaRuntimeMismatch,
            "runtime mismatch",
            "/java8/bin/java",
            &[],
            false,
            false,
        );

        assert!(matches!(
            decision,
            PreLaunchDecision::Intervene {
                action: PreLaunchAction::ForceManagedRuntime,
                ..
            }
        ));
    }

    #[test]
    fn custom_mode_blocks_startup_preset_healing() {
        let info = JavaRuntimeInfo {
            id: "test".to_string(),
            major: 17,
            update: 0,
            distribution: "temurin".to_string(),
            path: "/usr/bin/java".to_string(),
        };
        let context = LaunchGuardianContext {
            mode: GuardianMode::Custom,
            java_override_origin: None,
            preset_override_origin: Some(OverrideOrigin::Instance),
            raw_jvm_args_origin: None,
        };

        let plan = recovery_plan_for_startup_failure(
            LaunchFailureClass::JvmUnsupportedOption,
            "1.20.4",
            &info,
            "",
            &context,
            false,
            "smooth",
        );

        assert!(plan.is_none());
    }

    #[test]
    fn managed_mode_allows_runtime_recovery_plan() {
        let info = JavaRuntimeInfo {
            id: "test".to_string(),
            major: 21,
            update: 0,
            distribution: "temurin".to_string(),
            path: "/usr/bin/java".to_string(),
        };
        let context = LaunchGuardianContext {
            mode: GuardianMode::Managed,
            java_override_origin: Some(OverrideOrigin::Instance),
            preset_override_origin: None,
            raw_jvm_args_origin: None,
        };

        let plan = recovery_plan_for_startup_failure(
            LaunchFailureClass::JavaRuntimeMismatch,
            "1.20.4",
            &info,
            "/java8/bin/java",
            &context,
            false,
            "",
        )
        .expect("expected plan");

        assert!(matches!(plan.action, RecoveryAction::SwitchManagedRuntime));
    }

    #[test]
    fn allowed_guardian_summary_has_no_user_facing_outcome() {
        let summary = GuardianSummary::new(GuardianMode::Managed);
        let serialized = serde_json::to_value(summary).expect("serialized summary");

        assert_eq!(serialized["decision"], json!("allowed"));
        assert!(serialized.get("message").is_none());
        assert!(serialized.get("details").is_none());
    }

    #[test]
    fn intervention_populates_backend_authored_outcome() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.record_intervention(
            GuardianInterventionKind::DowngradePreset,
            "Guardian downgraded JVM preset from \"graalvm\" to \"performance\" before launch",
            false,
        );

        assert_eq!(summary.decision, super::GuardianDecision::Intervened);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian adjusted launch settings for safety.")
        );
        assert_eq!(
            summary.details,
            vec!["JVM preset changed from GraalVM to Performance for compatibility."]
        );
    }

    #[test]
    fn blocked_summary_prefers_guardian_message_and_guidance_details() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.block_with_guidance(vec![
            "Use a compatible Java runtime or let Croopor use the managed runtime.".to_string(),
        ]);

        assert_eq!(summary.decision, super::GuardianDecision::Blocked);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert_eq!(
            summary.details,
            vec!["Use a compatible Java runtime or let Croopor use the managed runtime."]
        );
    }

    #[test]
    fn warned_summary_populates_backend_authored_outcome() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.warn_with_guidance(vec![
            "Review custom launch settings before retrying.".to_string(),
        ]);

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian flagged launch settings for review.")
        );
        assert_eq!(
            summary.details,
            vec!["Review custom launch settings before retrying."]
        );
    }

    #[test]
    fn warned_summary_merges_guidance_without_duplicates() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.warn_with_guidance(vec!["Launch memory budget is tight.".to_string()]);
        summary.warn_with_guidance(vec![
            "Launch memory budget is tight.".to_string(),
            "Review custom launch settings before retrying.".to_string(),
        ]);

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_eq!(
            summary.guidance,
            vec![
                "Launch memory budget is tight.",
                "Review custom launch settings before retrying.",
            ]
        );
        assert_eq!(summary.details, summary.guidance);
    }

    #[test]
    fn silent_intervention_keeps_detail_out_of_user_facing_outcome() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.record_intervention(
            GuardianInterventionKind::SwitchManagedRuntime,
            "internal runtime adjustment",
            true,
        );

        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian adjusted launch settings for safety.")
        );
        assert!(summary.details.is_empty());
    }

    #[test]
    fn conservative_preset_uses_legacy_for_alpha_and_beta_versions() {
        let info = JavaRuntimeInfo {
            id: "test".to_string(),
            major: 17,
            update: 0,
            distribution: "temurin".to_string(),
            path: "/usr/bin/java".to_string(),
        };

        assert_eq!(conservative_healing_preset("b1.8.1", &info), "legacy");
        assert_eq!(conservative_healing_preset("a1.2.6", &info), "legacy");
    }
}
