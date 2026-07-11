use crate::crash::is_out_of_memory_failure_line;
use crate::types::LaunchFailureClass;
use serde::{Deserialize, Serialize};

pub const LAUNCH_MEMORY_HEADROOM_MB: u64 = 2048;
pub const LAUNCH_DISK_HEADROOM_MB: u64 = 2048;
pub const LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB: i32 = 2048;

#[cfg(test)]
const MEMORY_CLAMP_WARNING: &str = "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.";
#[cfg(test)]
const MEMORY_CLAMP_GUIDANCE: &str = "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchCpuLoadWarningFacts {
    pub host_cpu_load_1m_x100: Option<u64>,
    pub host_cpu_load_5m_x100: Option<u64>,
    pub host_cpu_load_15m_x100: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchResourceWarningFacts {
    pub host_total_memory_mb: Option<u64>,
    pub host_cpu_threads: Option<usize>,
    pub cpu_load: LaunchCpuLoadWarningFacts,
    pub active_session_count: usize,
    pub active_install_count: usize,
    pub active_memory_allocation_mb: u64,
    pub requested_memory_mb: Option<i32>,
    pub launch_disk_available_mb: Option<u64>,
    pub memory_headroom_mb: u64,
    pub launch_disk_headroom_mb: u64,
}

impl Default for LaunchResourceWarningFacts {
    fn default() -> Self {
        Self {
            host_total_memory_mb: None,
            host_cpu_threads: None,
            cpu_load: LaunchCpuLoadWarningFacts::default(),
            active_session_count: 0,
            active_install_count: 0,
            active_memory_allocation_mb: 0,
            requested_memory_mb: None,
            launch_disk_available_mb: None,
            memory_headroom_mb: LAUNCH_MEMORY_HEADROOM_MB,
            launch_disk_headroom_mb: LAUNCH_DISK_HEADROOM_MB,
        }
    }
}

impl LaunchResourceWarningFacts {
    pub fn memory_pressure(self) -> bool {
        let Some(total_memory_mb) = self.host_total_memory_mb else {
            return false;
        };
        let Some(requested_memory_mb) = self
            .requested_memory_mb
            .and_then(|value| u64::try_from(value).ok())
        else {
            return false;
        };
        let remaining_mb = total_memory_mb.saturating_sub(
            self.active_memory_allocation_mb
                .saturating_add(requested_memory_mb),
        );
        remaining_mb < self.memory_headroom_mb
    }

    pub fn cpu_pressure(self) -> bool {
        active_launch_cpu_pressure(self.host_cpu_threads, self.active_session_count)
            || measured_cpu_load_pressure(self.host_cpu_threads, self.cpu_load)
    }

    pub fn install_pressure(self) -> bool {
        self.active_install_count > 0
    }

    pub fn disk_pressure(self) -> bool {
        self.launch_disk_available_mb
            .is_some_and(|available_mb| available_mb < self.launch_disk_headroom_mb)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg(test)]
struct LaunchWarningFacts {
    pub raw_min_memory_mb: i32,
    pub max_memory_mb: i32,
    pub resource: LaunchResourceWarningFacts,
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
fn summarize_launch_warnings(
    context: &LaunchGuardianContext,
    facts: &LaunchWarningFacts,
) -> GuardianSummary {
    let mut summary = GuardianSummary::new(context.mode);
    for guidance in [
        memory_clamp_warning_guidance(facts.raw_min_memory_mb, facts.max_memory_mb),
        low_memory_allocation_warning_guidance(facts.max_memory_mb),
        memory_budget_warning_guidance(facts.resource),
        cpu_pressure_warning_guidance(facts.resource),
        install_pressure_warning_guidance(facts.resource),
        disk_pressure_warning_guidance(facts.resource),
        custom_risky_override_warning_guidance(context),
    ]
    .into_iter()
    .flatten()
    {
        summary.warn_with_guidance(guidance);
    }
    summary
}

#[cfg(test)]
fn memory_clamp_warning_guidance(
    raw_min_memory_mb: i32,
    max_memory_mb: i32,
) -> Option<Vec<String>> {
    (raw_min_memory_mb > max_memory_mb).then(|| {
        vec![
            MEMORY_CLAMP_WARNING.to_string(),
            MEMORY_CLAMP_GUIDANCE.to_string(),
        ]
    })
}

#[cfg(test)]
fn low_memory_allocation_warning_guidance(max_memory_mb: i32) -> Option<Vec<String>> {
    (max_memory_mb > 0 && max_memory_mb < LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB).then(|| {
        vec![
            format!(
                "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM ({max_memory_mb} MB selected)."
            ),
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
        ]
    })
}

#[cfg(test)]
fn memory_budget_warning_guidance(resource: LaunchResourceWarningFacts) -> Option<Vec<String>> {
    if !resource.memory_pressure() {
        return None;
    }
    Some(vec![
        "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.".to_string(),
        "Close another running session or lower this instance's memory allocation if startup or gameplay becomes unstable.".to_string(),
    ])
}

#[cfg(test)]
fn cpu_pressure_warning_guidance(resource: LaunchResourceWarningFacts) -> Option<Vec<String>> {
    if !resource.cpu_pressure() {
        return None;
    }
    let cpu_threads = resource.host_cpu_threads?;
    let load_pressure =
        measured_cpu_load_pressure_evidence(resource.host_cpu_threads, resource.cpu_load);
    let launch_pressure =
        active_launch_cpu_pressure(resource.host_cpu_threads, resource.active_session_count);
    let mut guidance = Vec::new();

    if let Some(load_pressure) = load_pressure {
        guidance.push(format!(
            "Host CPU load is already high: {} load average is {} on {cpu_threads} CPU threads before launch.",
            load_pressure.window_label,
            format_load_x100(load_pressure.load_x100)
        ));
        guidance.push(
            "Close CPU-heavy apps or wait for background work to settle if startup feels sluggish."
                .to_string(),
        );
    }

    if launch_pressure {
        guidance.push(format!(
            "Launch concurrency may be tight: this device reports {cpu_threads} CPU threads, and other active launch sessions before this one: {}.",
            resource.active_session_count
        ));
        guidance.push(
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.".to_string(),
        );
    }

    (!guidance.is_empty()).then_some(guidance)
}

#[cfg(test)]
fn install_pressure_warning_guidance(resource: LaunchResourceWarningFacts) -> Option<Vec<String>> {
    if !resource.install_pressure() {
        return None;
    }
    Some(vec![
        format!(
            "Active install/download sessions: {}. Launching now can add disk and network pressure during startup.",
            resource.active_install_count
        ),
        "On low-end devices, wait for the install or download to finish if startup feels slow."
            .to_string(),
    ])
}

#[cfg(test)]
fn disk_pressure_warning_guidance(resource: LaunchResourceWarningFacts) -> Option<Vec<String>> {
    if !resource.disk_pressure() {
        return None;
    }
    let available_mb = resource.launch_disk_available_mb?;

    Some(vec![
        format!(
            "Launch disk space is tight: launch-relevant storage reports less than 2 GB free ({available_mb} MB available)."
        ),
        "Free disk space before launching if caches, natives, or prewarm steps become unreliable."
            .to_string(),
    ])
}

#[cfg(test)]
fn custom_risky_override_warning_guidance(context: &LaunchGuardianContext) -> Option<Vec<String>> {
    if !matches!(context.mode, GuardianMode::Custom) || !context.has_risky_overrides() {
        return None;
    }

    let mut guidance = Vec::new();
    if context.has_java_override() {
        guidance.push(
            "Guardian Custom mode will keep the selected Java override for this launch."
                .to_string(),
        );
    }
    if context.has_named_preset() {
        guidance.push(
            "Guardian Custom mode will keep the selected JVM preset for this launch.".to_string(),
        );
    }
    if context.has_raw_jvm_args() {
        guidance.push(
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."
                .to_string(),
        );
    }
    guidance.push(
        "Switch Guardian back to Managed if you want Axial to adjust unsafe choices.".to_string(),
    );
    Some(guidance)
}

fn active_launch_cpu_pressure(cpu_threads: Option<usize>, active_launch_count: usize) -> bool {
    let Some(cpu_threads) = cpu_threads.filter(|value| *value > 0) else {
        return false;
    };
    let queued_launch_count = active_launch_count.saturating_add(1);
    if cpu_threads <= 4 {
        active_launch_count >= 1
    } else if cpu_threads <= 8 {
        queued_launch_count >= 3
    } else {
        queued_launch_count >= 5
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuLoadPressureEvidence {
    window_label: &'static str,
    load_x100: u64,
}

fn measured_cpu_load_pressure(
    cpu_threads: Option<usize>,
    cpu_load: LaunchCpuLoadWarningFacts,
) -> bool {
    measured_cpu_load_pressure_evidence(cpu_threads, cpu_load).is_some()
}

fn measured_cpu_load_pressure_evidence(
    cpu_threads: Option<usize>,
    cpu_load: LaunchCpuLoadWarningFacts,
) -> Option<CpuLoadPressureEvidence> {
    let cpu_threads = cpu_threads.filter(|value| *value > 0)?;
    let sample = most_recent_cpu_load_sample(cpu_load)?;
    let threshold_x100 = measured_cpu_load_threshold_x100(cpu_threads);
    (sample.load_x100 >= threshold_x100).then_some(sample)
}

fn most_recent_cpu_load_sample(
    cpu_load: LaunchCpuLoadWarningFacts,
) -> Option<CpuLoadPressureEvidence> {
    cpu_load
        .host_cpu_load_1m_x100
        .map(|load_x100| CpuLoadPressureEvidence {
            window_label: "1-minute",
            load_x100,
        })
        .or_else(|| {
            cpu_load
                .host_cpu_load_5m_x100
                .map(|load_x100| CpuLoadPressureEvidence {
                    window_label: "5-minute",
                    load_x100,
                })
        })
        .or_else(|| {
            cpu_load
                .host_cpu_load_15m_x100
                .map(|load_x100| CpuLoadPressureEvidence {
                    window_label: "15-minute",
                    load_x100,
                })
        })
}

fn measured_cpu_load_threshold_x100(cpu_threads: usize) -> u64 {
    let headroom_percent = if cpu_threads <= 4 {
        75_u64
    } else if cpu_threads <= 8 {
        85
    } else {
        95
    };
    u64::try_from(cpu_threads)
        .unwrap_or(u64::MAX / 100)
        .saturating_mul(headroom_percent)
}

#[cfg(test)]
fn format_load_x100(value: u64) -> String {
    format!("{}.{:02}", value / 100, value % 100)
}

pub fn classify_startup_failure_text(text: &str) -> LaunchFailureClass {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return LaunchFailureClass::Unknown;
    }
    if lower.contains("unrecognized vm option") || lower.contains("unsupported vm option") {
        return LaunchFailureClass::JvmUnsupportedOption;
    }
    if lower.contains("must be enabled via -xx:+unlockexperimentalvmoptions") {
        return LaunchFailureClass::JvmExperimentalUnlock;
    }
    if lower.contains("unlock option must precede") || lower.contains("must precede") {
        return LaunchFailureClass::JvmOptionOrdering;
    }
    if lower.contains("unsupportedclassversionerror")
        || lower.contains("compiled by a more recent version of the java runtime")
        || lower.contains("requires java")
    {
        return LaunchFailureClass::JavaRuntimeMismatch;
    }
    if contains_out_of_memory_failure(&lower) {
        return LaunchFailureClass::OutOfMemory;
    }
    if contains_artifact_signature_failure(&lower) {
        return LaunchFailureClass::LauncherManagedArtifactSignature;
    }
    if lower.contains("resolutionexception: modules")
        || lower.contains("export package")
        || lower.contains("modulelayerhandler.buildlayer")
        || lower.contains("noclassdeffounderror")
        || lower.contains("classnotfoundexception")
        || lower.contains("failed to locate library:")
        || lower.contains("unsatisfiedlinkerror")
    {
        return LaunchFailureClass::ClasspathModuleConflict;
    }
    if lower.contains("nosuchelementexception: no value present")
        || (contains_loader_bootstrap_marker(&lower) && contains_failure_context(&lower))
    {
        return LaunchFailureClass::LoaderBootstrapFailure;
    }
    if lower.contains("microsoft account")
        || lower.contains("check your microsoft account")
        || lower.contains("multiplayer is disabled")
    {
        return LaunchFailureClass::AuthModeIncompatible;
    }
    LaunchFailureClass::Unknown
}

fn contains_out_of_memory_failure(text: &str) -> bool {
    text.lines().any(is_out_of_memory_failure_line)
}

fn contains_artifact_signature_failure(text: &str) -> bool {
    text.contains("invalid signature file digest")
        || (text.contains("securityexception")
            && text.contains("signer information does not match")
            && text.contains("same package"))
        || (text.contains("securityexception")
            && text.contains("signature file")
            && text.contains("digest"))
        || (text.contains("securityexception")
            && text.contains("manifest main attributes")
            && text.contains("digest"))
        || (text.contains("securityexception") && text.contains("digest error"))
}

fn contains_loader_bootstrap_marker(text: &str) -> bool {
    text.contains("bootstraplauncher")
        || text.contains("modlauncher")
        || text.contains("fml loading")
}

fn contains_failure_context(text: &str) -> bool {
    text.contains("exception")
        || text.contains("error")
        || text.contains("fail")
        || text.contains("unable")
        || text.contains("could not")
}

#[cfg(test)]
mod tests {
    use super::{
        GuardianInterventionKind, GuardianMode, GuardianSummary,
        LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB, LaunchCpuLoadWarningFacts,
        LaunchGuardianContext, LaunchResourceWarningFacts, LaunchWarningFacts, OverrideOrigin,
        classify_startup_failure_text, summarize_launch_warnings,
    };
    use crate::types::LaunchFailureClass;
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
    fn startup_failure_text_classification_is_bounded_to_failure_class() {
        assert_eq!(
            classify_startup_failure_text(
                "Unrecognized VM option '-XX:+UseZGC' in /home/alice/.axial/instances/secret"
            ),
            LaunchFailureClass::JvmUnsupportedOption
        );
        assert_eq!(
            classify_startup_failure_text(
                "java.lang.UnsupportedClassVersionError: compiled by a more recent version of the Java Runtime"
            ),
            LaunchFailureClass::JavaRuntimeMismatch
        );
        assert_eq!(
            classify_startup_failure_text(
                "Caused by: java.lang.NoClassDefFoundError: org/lwjgl/glfw/GLFW"
            ),
            LaunchFailureClass::ClasspathModuleConflict
        );
        assert_eq!(
            classify_startup_failure_text(
                "java.lang.UnsatisfiedLinkError: Failed to locate library: lwjgl.dll"
            ),
            LaunchFailureClass::ClasspathModuleConflict
        );
        assert_eq!(
            classify_startup_failure_text(
                "java.lang.SecurityException: class net.minecraft.SomeClass signer information does not match signer information of other classes in the same package"
            ),
            LaunchFailureClass::LauncherManagedArtifactSignature
        );
        assert_eq!(
            classify_startup_failure_text(
                "Exception in thread \"main\" java.lang.SecurityException: Invalid signature file digest for Manifest main attributes"
            ),
            LaunchFailureClass::LauncherManagedArtifactSignature
        );
        assert_eq!(
            classify_startup_failure_text(
                "java.lang.SecurityException: SHA-256 digest error for net/minecraft/client/Minecraft.class"
            ),
            LaunchFailureClass::LauncherManagedArtifactSignature
        );
        assert_eq!(
            classify_startup_failure_text(
                "[main/INFO] [cpw.mods.modlauncher.Launcher/MODLAUNCHER]: ModLauncher running"
            ),
            LaunchFailureClass::Unknown
        );
        assert_eq!(
            classify_startup_failure_text(
                "[EARLYDISPLAY/]: If this message is the only thing at the bottom of your log before a crash, you probably have a driver issue."
            ),
            LaunchFailureClass::Unknown
        );
        assert_eq!(
            classify_startup_failure_text(
                "cpw.mods.modlauncher.api.IncompatibleEnvironmentException: failed to load transformation service"
            ),
            LaunchFailureClass::LoaderBootstrapFailure
        );
        assert_eq!(
            classify_startup_failure_text("ordinary launcher output"),
            LaunchFailureClass::Unknown
        );
    }

    #[test]
    fn startup_failure_text_classifies_out_of_memory_signatures() {
        for output in [
            "Exception in thread \"Render thread\" java.lang.OutOfMemoryError: Java heap space",
            "java.lang.OutOfMemoryError: GC overhead limit exceeded",
            "GC overhead limit exceeded",
            "# There is insufficient memory for the Java Runtime Environment to continue.",
            "# Native memory allocation (malloc) failed to allocate 1048576 bytes. Error detail: AllocateHeap",
            "# Native memory allocation (mmap) failed to map 65536 bytes. Error detail: committing reserved memory.",
            "# Out of Memory Error (allocation.cpp:44)",
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                LaunchFailureClass::OutOfMemory,
                "expected OOM classification for {output:?}"
            );
        }
    }

    #[test]
    fn startup_failure_text_rejects_incidental_memory_text() {
        for output in [
            "[main/INFO] Loading MemoryLeakFix 1.1.5 and ModernFix",
            "[main/INFO] Allocated memory: 4096 MiB",
            "Memory settings saved successfully",
            "Native memory allocation completed",
            "Out of Memory Error is the title of this troubleshooting guide",
            "# Out of Memory Error handling is enabled",
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                LaunchFailureClass::Unknown,
                "unexpected OOM classification for {output:?}"
            );
        }
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
    fn launch_warning_summary_reports_memory_clamp() {
        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                raw_min_memory_mb: 2048,
                max_memory_mb: 1024,
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        );
        assert_has_guidance(
            &summary,
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        );
    }

    #[test]
    fn launch_warning_summary_reports_low_max_allocation_only_below_threshold() {
        let clear = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB,
                ..LaunchWarningFacts::default()
            },
        );
        assert_eq!(clear.decision, super::GuardianDecision::Allowed);

        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: LOW_MEMORY_ALLOCATION_WARNING_THRESHOLD_MB - 1,
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Launch memory allocation is very low: this instance is limited to less than 2 GB of RAM (2047 MB selected).",
        );
        assert_has_guidance(
            &summary,
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.",
        );
    }

    #[test]
    fn launch_warning_summary_reports_memory_pressure() {
        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: 4096,
                resource: LaunchResourceWarningFacts {
                    host_total_memory_mb: Some(8192),
                    active_memory_allocation_mb: 3072,
                    requested_memory_mb: Some(4096),
                    ..LaunchResourceWarningFacts::default()
                },
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Launch memory budget is tight: active sessions plus this launch may leave less than 2 GB for the OS.",
        );
        assert_has_guidance(
            &summary,
            "Close another running session or lower this instance's memory allocation if startup or gameplay becomes unstable.",
        );
    }

    #[test]
    fn launch_warning_summary_reports_cpu_concurrency_pressure() {
        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: 4096,
                resource: LaunchResourceWarningFacts {
                    host_cpu_threads: Some(4),
                    active_session_count: 1,
                    ..LaunchResourceWarningFacts::default()
                },
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Launch concurrency may be tight: this device reports 4 CPU threads, and other active launch sessions before this one: 1.",
        );
        assert_has_guidance(
            &summary,
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
        );
    }

    #[test]
    fn launch_warning_summary_reports_most_recent_cpu_load_pressure() {
        let clear = LaunchResourceWarningFacts {
            host_cpu_threads: Some(4),
            cpu_load: LaunchCpuLoadWarningFacts {
                host_cpu_load_1m_x100: Some(299),
                host_cpu_load_5m_x100: Some(300),
                host_cpu_load_15m_x100: Some(300),
            },
            ..LaunchResourceWarningFacts::default()
        };
        assert!(!clear.cpu_pressure());

        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: 4096,
                resource: LaunchResourceWarningFacts {
                    host_cpu_threads: Some(4),
                    cpu_load: LaunchCpuLoadWarningFacts {
                        host_cpu_load_1m_x100: None,
                        host_cpu_load_5m_x100: Some(300),
                        host_cpu_load_15m_x100: Some(100),
                    },
                    ..LaunchResourceWarningFacts::default()
                },
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Host CPU load is already high: 5-minute load average is 3.00 on 4 CPU threads before launch.",
        );
        assert_has_guidance(
            &summary,
            "Close CPU-heavy apps or wait for background work to settle if startup feels sluggish.",
        );
    }

    #[test]
    fn launch_warning_summary_reports_install_and_disk_pressure() {
        let summary = summarize_launch_warnings(
            &managed_context(),
            &LaunchWarningFacts {
                max_memory_mb: 4096,
                resource: LaunchResourceWarningFacts {
                    active_install_count: 2,
                    launch_disk_available_mb: Some(1400),
                    ..LaunchResourceWarningFacts::default()
                },
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Active install/download sessions: 2. Launching now can add disk and network pressure during startup.",
        );
        assert_has_guidance(
            &summary,
            "Launch disk space is tight: launch-relevant storage reports less than 2 GB free (1400 MB available).",
        );
        assert_has_guidance(
            &summary,
            "Free disk space before launching if caches, natives, or prewarm steps become unreliable.",
        );
    }

    #[test]
    fn launch_warning_summary_merges_custom_override_warning_with_existing_guidance() {
        let context = LaunchGuardianContext {
            mode: GuardianMode::Custom,
            java_override_origin: Some(OverrideOrigin::Instance),
            preset_override_origin: Some(OverrideOrigin::Global),
            raw_jvm_args_origin: Some(OverrideOrigin::Instance),
        };
        let summary = summarize_launch_warnings(
            &context,
            &LaunchWarningFacts {
                raw_min_memory_mb: 2048,
                max_memory_mb: 1024,
                ..LaunchWarningFacts::default()
            },
        );

        assert_eq!(summary.decision, super::GuardianDecision::Warned);
        assert_has_guidance(
            &summary,
            "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        );
        assert_has_guidance(
            &summary,
            "Guardian Custom mode will keep the selected Java override for this launch.",
        );
        assert_has_guidance(
            &summary,
            "Guardian Custom mode will keep the selected JVM preset for this launch.",
        );
        assert_has_guidance(
            &summary,
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.",
        );
        assert_has_guidance(
            &summary,
            "Switch Guardian back to Managed if you want Axial to adjust unsafe choices.",
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

    fn managed_context() -> LaunchGuardianContext {
        LaunchGuardianContext {
            mode: GuardianMode::Managed,
            ..LaunchGuardianContext::default()
        }
    }

    fn assert_has_guidance(summary: &GuardianSummary, expected: &str) {
        assert!(
            summary.guidance.iter().any(|detail| detail == expected),
            "missing guidance: {expected}"
        );
        assert!(
            summary.details.iter().any(|detail| detail == expected),
            "missing detail: {expected}"
        );
    }
}
