use serde::{Deserialize, Serialize};

pub const LAUNCH_MEMORY_HEADROOM_MB: u64 = 2048;
pub const LAUNCH_DISK_HEADROOM_MB: u64 = 2048;

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
        self.block_with_reason_and_guidance("", guidance);
    }

    pub fn block_with_reason_and_guidance(
        &mut self,
        reason: impl Into<String>,
        guidance: Vec<String>,
    ) {
        self.decision = GuardianDecision::Blocked;
        self.guidance = guidance;
        self.refresh_outcome();
        prepend_unique_detail(&mut self.details, Some(reason.into()));
    }

    pub fn block_with_message_reason_and_guidance(
        &mut self,
        message: impl Into<String>,
        reason: impl Into<String>,
        guidance: Vec<String>,
    ) {
        self.block_with_reason_and_guidance(reason, guidance);
        let message = message.into();
        if !message.trim().is_empty() {
            self.message = Some(message.trim().to_string());
        }
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

fn prepend_unique_detail(details: &mut Vec<String>, detail: Option<String>) {
    let Some(detail) = detail else {
        return;
    };
    let detail = detail.trim();
    if detail.is_empty() {
        return;
    }
    details.retain(|existing| existing != detail);
    details.insert(0, detail.to_string());
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

#[cfg(test)]
mod tests {
    use super::{
        GuardianInterventionKind, GuardianMode, GuardianSummary, LaunchGuardianContext,
        OverrideOrigin,
    };
    use serde_json::json;

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
            "Use a compatible Java runtime or let Axial use the managed runtime.".to_string(),
        ]);

        assert_eq!(summary.decision, super::GuardianDecision::Blocked);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert_eq!(
            summary.details,
            vec!["Use a compatible Java runtime or let Axial use the managed runtime."]
        );
    }

    #[test]
    fn blocked_summary_with_reason_orders_reason_before_deduped_guidance() {
        let mut summary = GuardianSummary::new(GuardianMode::Managed);
        summary.block_with_reason_and_guidance(
            " explicit Java override targets Java 8 but this version requires Java 17 ",
            vec![
                "Remove the Java override or switch Guardian Mode back to Managed.".to_string(),
                "explicit Java override targets Java 8 but this version requires Java 17"
                    .to_string(),
                "Remove the Java override or switch Guardian Mode back to Managed.".to_string(),
            ],
        );

        assert_eq!(summary.decision, super::GuardianDecision::Blocked);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian blocked an unsafe launch setup.")
        );
        assert_eq!(
            summary.details,
            vec![
                "explicit Java override targets Java 8 but this version requires Java 17",
                "Remove the Java override or switch Guardian Mode back to Managed.",
            ]
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
}
