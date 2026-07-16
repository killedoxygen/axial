use super::jvm_preset::{GuardianJvmPresetId, GuardianJvmPresetResolution};
use super::{
    DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus, GuardianDirective, GuardianFact,
    GuardianFactId, GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
    GuardianManagedJavaReason, GuardianMode, GuardianObservedLaunchFailurePhase,
    GuardianPerformanceSupervisionRejection, GuardianPreflightOutcome,
    GuardianPresetDowngradeReason, GuardianPresetValue, GuardianRepairStatus,
    GuardianStartupFailureObservation, GuardianStartupFailureRequest, GuardianStripJvmArgsReason,
};
use crate::observability::{
    RedactionAudience, sanitize_evidence_text, sanitize_evidence_token,
    sanitize_public_diagnostic_text,
};
use crate::state::contracts::OperationPhase;
use axial_launcher::{
    CrashEvidence, GuardianMode as LauncherGuardianMode, HealingEventKind, LaunchFailureClass,
    LaunchHealingSummary, LaunchNotice, LaunchNoticeTone, LaunchSessionExitReason,
    LaunchSessionOutcome, LaunchSessionOutcomeKind, LaunchSessionRecord, LaunchStageEvidence,
    LaunchStatusEvent,
};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuardianSummaryDecision {
    Allowed,
    Warned,
    Blocked,
    Intervened,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GuardianInterventionKind {
    SwitchManagedRuntime,
    StripJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GuardianIntervention {
    kind: GuardianInterventionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    silent: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuardianSummary {
    mode: LauncherGuardianMode,
    decision: GuardianSummaryDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    details: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    guidance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    interventions: Vec<GuardianIntervention>,
}

impl GuardianSummary {
    #[cfg(test)]
    pub(crate) const fn mode(&self) -> LauncherGuardianMode {
        self.mode
    }

    pub(crate) const fn decision(&self) -> GuardianSummaryDecision {
        self.decision
    }

    #[cfg(test)]
    pub(crate) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn details(&self) -> &[String] {
        &self.details
    }

    #[cfg(test)]
    pub(crate) fn guidance(&self) -> &[String] {
        &self.guidance
    }

    #[cfg(test)]
    pub(crate) fn has_interventions(&self) -> bool {
        !self.interventions.is_empty()
    }
}

#[derive(Deserialize)]
struct PersistedGuardianSummary {
    mode: LauncherGuardianMode,
    decision: PersistedGuardianSummaryDecision,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    details: Vec<String>,
    #[serde(default)]
    guidance: Vec<String>,
    #[serde(default)]
    interventions: Vec<PersistedGuardianIntervention>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedGuardianIntervention {
    kind: PersistedGuardianInterventionKind,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    public_detail: Option<String>,
    #[serde(default)]
    silent: Option<bool>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedGuardianSummaryDecision {
    Allowed,
    Warned,
    Blocked,
    Intervened,
}

impl From<PersistedGuardianSummaryDecision> for GuardianSummaryDecision {
    fn from(decision: PersistedGuardianSummaryDecision) -> Self {
        match decision {
            PersistedGuardianSummaryDecision::Allowed => Self::Allowed,
            PersistedGuardianSummaryDecision::Warned => Self::Warned,
            PersistedGuardianSummaryDecision::Blocked => Self::Blocked,
            PersistedGuardianSummaryDecision::Intervened => Self::Intervened,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedGuardianInterventionKind {
    SwitchManagedRuntime,
    StripJvmArgs,
    DowngradePreset,
    DisableCustomGc,
}

impl From<PersistedGuardianInterventionKind> for GuardianInterventionKind {
    fn from(kind: PersistedGuardianInterventionKind) -> Self {
        match kind {
            PersistedGuardianInterventionKind::SwitchManagedRuntime => Self::SwitchManagedRuntime,
            PersistedGuardianInterventionKind::StripJvmArgs => Self::StripJvmArgs,
            PersistedGuardianInterventionKind::DowngradePreset => Self::DowngradePreset,
            PersistedGuardianInterventionKind::DisableCustomGc => Self::DisableCustomGc,
        }
    }
}

const MAX_SUMMARY_BYTES: usize = 180;
const MAX_LINE_BYTES: usize = 240;
const MAX_COLLECTION_LINES: usize = 6;
const MAX_EXPORT_SUMMARY_BYTES: usize = 180;
const MAX_EXPORT_LINE_BYTES: usize = 180;
const MAX_EXPORT_COLLECTION_LINES: usize = 8;
const MAX_DYNAMIC_TOKEN_BYTES: usize = 64;
const MAX_STAGE_SUMMARY_BYTES: usize = 160;
const MAX_STAGE_DETAIL_BYTES: usize = 120;
const MAX_PROOF_DETAIL_BYTES: usize = 150;
const MAX_NOTICE_MESSAGE_BYTES: usize = 180;
const MAX_NOTICE_DETAIL_BYTES: usize = 240;
const MAX_NOTICE_DETAILS: usize = 8;
const PRIVATE_NOTICE_FALLBACK: &str = "Launch status details were hidden for privacy.";
const GUARDIAN_OUTCOME_DECISION_PREFIX: &str = "guardian_outcome_decision:";
const GUARDIAN_OUTCOME_SUMMARY_PREFIX: &str = "guardian_outcome_summary:";
const GUARDIAN_OUTCOME_DETAIL_PREFIX: &str = "guardian_outcome_detail:";

pub(crate) fn guardian_summary_from_persisted_value(value: &Value) -> Option<GuardianSummary> {
    guardian_summary_from_persisted_value_with_limits(
        value,
        RedactionAudience::UserVisible,
        MAX_SUMMARY_BYTES,
        MAX_LINE_BYTES,
        MAX_COLLECTION_LINES,
    )
}

pub(crate) fn guardian_summary_from_persisted_export_value(
    value: &Value,
) -> Option<GuardianSummary> {
    guardian_summary_from_persisted_value_with_limits(
        value,
        RedactionAudience::ExportableProof,
        MAX_EXPORT_SUMMARY_BYTES,
        MAX_EXPORT_LINE_BYTES,
        MAX_EXPORT_COLLECTION_LINES,
    )
}

fn guardian_summary_from_persisted_value_with_limits(
    value: &Value,
    audience: RedactionAudience,
    max_summary_bytes: usize,
    max_line_bytes: usize,
    max_collection_lines: usize,
) -> Option<GuardianSummary> {
    let persisted = serde_json::from_value::<PersistedGuardianSummary>(value.clone()).ok()?;
    let message = persisted
        .message
        .as_deref()
        .and_then(|message| sanitize_evidence_text(message, audience, max_summary_bytes));
    let details = persisted
        .details
        .iter()
        .filter_map(|detail| sanitize_evidence_text(detail, audience, max_line_bytes))
        .take(max_collection_lines)
        .collect();
    let guidance = persisted
        .guidance
        .iter()
        .filter_map(|detail| sanitize_evidence_text(detail, audience, max_line_bytes))
        .take(max_collection_lines)
        .collect();
    let interventions = persisted
        .interventions
        .into_iter()
        .map(|intervention| GuardianIntervention {
            kind: intervention.kind.into(),
            detail: intervention
                .detail
                .as_deref()
                .and_then(|detail| sanitize_evidence_text(detail, audience, max_line_bytes)),
            public_detail: intervention
                .public_detail
                .as_deref()
                .and_then(|detail| sanitize_evidence_text(detail, audience, max_line_bytes)),
            silent: intervention.silent,
        })
        .take(max_collection_lines)
        .collect();

    Some(GuardianSummary {
        mode: persisted.mode,
        decision: persisted.decision.into(),
        message,
        details,
        guidance,
        interventions,
    })
}

#[cfg(test)]
pub(crate) fn guardian_summary_for_test(
    mode: LauncherGuardianMode,
    decision: GuardianSummaryDecision,
    message: Option<String>,
    details: Vec<String>,
    guidance: Vec<String>,
    intervention_details: Vec<String>,
) -> GuardianSummary {
    GuardianSummary {
        mode,
        decision,
        message,
        details,
        guidance,
        interventions: intervention_details
            .into_iter()
            .map(|detail| GuardianIntervention {
                kind: GuardianInterventionKind::SwitchManagedRuntime,
                public_detail: Some(detail.clone()),
                detail: Some(detail),
                silent: Some(false),
            })
            .collect(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianJvmPresetOption {
    id: String,
    label: String,
    detail: String,
    default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianJvmPresetNotice {
    state_id: String,
    tone: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl GuardianJvmPresetNotice {
    pub fn state_id(&self) -> &str {
        &self.state_id
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardianJvmPresetCopyRule {
    preset: GuardianJvmPresetId,
    label: &'static str,
    detail: &'static str,
}

const GUARDIAN_JVM_PRESET_COPY_RULES: [GuardianJvmPresetCopyRule; 8] = [
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::Auto,
        label: "Auto",
        detail: "Axial picks safe JVM flags automatically.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::Smooth,
        label: "Smooth",
        detail: "Balances throughput and steady frame times.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::Performance,
        label: "Performance",
        detail: "Pushes higher throughput on modern hardware.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::UltraLowLatency,
        label: "Low latency",
        detail: "Shortens JVM pauses, sometimes trading peak FPS.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::GraalVm,
        label: "GraalVM",
        detail: "Uses flags intended for GraalVM-based Java runtimes.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::Legacy,
        label: "Legacy",
        detail: "Keeps conservative flags for older Minecraft and Java stacks.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::LegacyPvp,
        label: "Legacy PvP",
        detail: "Legacy tuning biased toward fast input response.",
    },
    GuardianJvmPresetCopyRule {
        preset: GuardianJvmPresetId::LegacyHeavy,
        label: "Legacy heavy",
        detail: "Legacy tuning for larger heaps and heavier old modpacks.",
    },
];

pub fn guardian_jvm_preset_options() -> Vec<GuardianJvmPresetOption> {
    GUARDIAN_JVM_PRESET_COPY_RULES
        .iter()
        .map(|rule| GuardianJvmPresetOption {
            id: rule.preset.as_str().to_string(),
            label: trusted_line(rule.label, MAX_SUMMARY_BYTES),
            detail: trusted_line(rule.detail, MAX_LINE_BYTES),
            default: rule.preset == GuardianJvmPresetId::Auto,
            disabled_reason: None,
        })
        .collect()
}

pub fn guardian_jvm_preset_notice(
    resolution: GuardianJvmPresetResolution,
) -> Option<GuardianJvmPresetNotice> {
    if resolution != GuardianJvmPresetResolution::UnknownResetToAutomatic {
        return None;
    }
    Some(GuardianJvmPresetNotice {
        state_id: "unknown_reset_to_auto".to_string(),
        tone: "warn".to_string(),
        message: trusted_line("Guardian adjusted the JVM preset", MAX_SUMMARY_BYTES),
        detail: Some(trusted_line(
            "Guardian reset an unknown JVM preset to Automatic so launch safety stays backend-owned.",
            MAX_LINE_BYTES,
        )),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardianLaunchStageEvidenceInput {
    mode: GuardianMode,
    decision: GuardianActionKind,
    diagnosis_count: usize,
}

impl From<&GuardianPreflightOutcome> for GuardianLaunchStageEvidenceInput {
    fn from(outcome: &GuardianPreflightOutcome) -> Self {
        Self {
            mode: outcome.guardian_decision.mode(),
            decision: outcome.user_outcome.decision(),
            diagnosis_count: outcome.safety_case.diagnoses.len(),
        }
    }
}

pub(crate) fn guardian_launch_stage_evidence(
    outcome: &GuardianPreflightOutcome,
) -> LaunchStageEvidence {
    author_guardian_launch_stage_evidence(outcome.into())
}

#[cfg(test)]
pub(crate) fn guardian_launch_stage_evidence_for_test(
    mode: GuardianMode,
    decision: GuardianActionKind,
    diagnosis_count: usize,
) -> LaunchStageEvidence {
    author_guardian_launch_stage_evidence(GuardianLaunchStageEvidenceInput {
        mode,
        decision,
        diagnosis_count,
    })
}

fn author_guardian_launch_stage_evidence(
    input: GuardianLaunchStageEvidenceInput,
) -> LaunchStageEvidence {
    LaunchStageEvidence {
        id: "guardian_launch_safety_decision".to_string(),
        system: "guardian".to_string(),
        summary: trusted_line(
            "Guardian recorded the launch safety decision.",
            MAX_STAGE_SUMMARY_BYTES,
        ),
        details: vec![
            checked_stage_detail(format!("mode:{}", guardian_mode_label(input.mode))),
            checked_stage_detail(format!(
                "decision:{}",
                guardian_action_label(input.decision)
            )),
            checked_stage_detail(format!("diagnoses:{}", input.diagnosis_count)),
        ],
    }
}

const fn guardian_mode_label(mode: GuardianMode) -> &'static str {
    match mode {
        GuardianMode::Managed => "Managed",
        GuardianMode::Custom => "Custom",
        GuardianMode::Disabled => "Disabled",
    }
}

const fn guardian_action_label(action: GuardianActionKind) -> &'static str {
    match action {
        GuardianActionKind::Allow => "Allow",
        GuardianActionKind::Warn => "Warn",
        GuardianActionKind::Repair => "Repair",
        GuardianActionKind::Retry => "Retry",
        GuardianActionKind::Strip => "Strip",
        GuardianActionKind::Downgrade => "Downgrade",
        GuardianActionKind::Fallback => "Fallback",
        GuardianActionKind::Quarantine => "Quarantine",
        GuardianActionKind::AskUser => "AskUser",
        GuardianActionKind::Block => "Block",
        GuardianActionKind::RecordOnly => "RecordOnly",
    }
}

fn checked_stage_detail(value: String) -> String {
    sanitize_evidence_text(
        &value,
        RedactionAudience::UserVisible,
        MAX_STAGE_DETAIL_BYTES,
    )
    .filter(|value| !value.is_empty() && value.len() <= MAX_STAGE_DETAIL_BYTES)
    .expect("typed Guardian stage detail must stay public and bounded")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct GuardianProofEvidenceProjection {
    tone: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

pub(crate) fn guardian_proof_evidence(
    guardian: &GuardianSummary,
) -> Option<GuardianProofEvidenceProjection> {
    let detail = first_bounded_proof_detail(
        guardian
            .message
            .iter()
            .cloned()
            .chain(guardian.details.iter().cloned())
            .chain(guardian.guidance.iter().cloned())
            .chain(
                guardian
                    .interventions
                    .iter()
                    .filter(|intervention| !intervention.silent.unwrap_or(false))
                    .filter_map(|intervention| intervention.public_detail.clone()),
            ),
    );
    let actionable = matches!(
        guardian.decision,
        GuardianSummaryDecision::Blocked
            | GuardianSummaryDecision::Warned
            | GuardianSummaryDecision::Intervened
    );
    if !actionable && detail.is_none() {
        return None;
    }

    let (tone, label) = match guardian.decision {
        GuardianSummaryDecision::Blocked => ("err", "Guardian blocked"),
        GuardianSummaryDecision::Warned => ("warn", "Guardian warned"),
        GuardianSummaryDecision::Intervened => ("info", "Guardian intervened"),
        GuardianSummaryDecision::Allowed => ("info", "Guardian note"),
    };
    Some(GuardianProofEvidenceProjection {
        tone: trusted_line(tone, MAX_SUMMARY_BYTES),
        label: trusted_line(label, MAX_SUMMARY_BYTES),
        detail,
    })
}

fn first_bounded_proof_detail(values: impl IntoIterator<Item = String>) -> Option<String> {
    values.into_iter().find_map(|value| {
        let detail = sanitize_public_diagnostic_text(
            &value,
            RedactionAudience::UserVisible,
            MAX_PROOF_DETAIL_BYTES,
            "",
        );
        (!detail.is_empty()).then_some(detail)
    })
}

pub(crate) fn launch_notice_from_values(
    guardian: Option<&Value>,
    healing: Option<&Value>,
    outcome: Option<&LaunchSessionOutcome>,
    lead_detail: Option<&str>,
    fallback_message: Option<&str>,
) -> Option<LaunchNotice> {
    let guardian = guardian.and_then(guardian_summary_from_persisted_value);
    let healing = healing.and_then(|value| serde_json::from_value(value.clone()).ok());
    launch_notice(
        guardian.as_ref(),
        healing.as_ref(),
        outcome,
        lead_detail,
        fallback_message,
    )
}

pub(crate) fn launch_notice(
    guardian: Option<&GuardianSummary>,
    healing: Option<&LaunchHealingSummary>,
    outcome: Option<&LaunchSessionOutcome>,
    lead_detail: Option<&str>,
    fallback_message: Option<&str>,
) -> Option<LaunchNotice> {
    let guardian_details = guardian_notice_details(guardian);
    let guardian_actionable = guardian.is_some_and(|guardian| {
        matches!(
            guardian.decision,
            GuardianSummaryDecision::Blocked
                | GuardianSummaryDecision::Warned
                | GuardianSummaryDecision::Intervened
        ) && (guardian
            .message
            .as_deref()
            .is_some_and(|message| !message.trim().is_empty())
            || !guardian_details.is_empty())
    });

    let mut details = Vec::new();
    if guardian_details.is_empty() {
        push_notice_detail(&mut details, lead_detail);
    }
    for detail in &guardian_details {
        push_notice_detail(&mut details, Some(detail));
    }
    if !guardian_actionable {
        for detail in healing_notice_details(healing) {
            push_notice_detail(&mut details, Some(&detail));
        }
    }
    if details.is_empty()
        && outcome.is_some_and(|outcome| {
            matches!(
                outcome.kind,
                LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown
            )
        })
    {
        push_notice_detail(
            &mut details,
            outcome.map(|outcome| outcome.summary.as_str()),
        );
    }

    let message = guardian_notice_message(guardian)
        .or_else(|| healing_notice_message(healing))
        .or_else(|| {
            outcome.and_then(|outcome| match outcome.kind {
                LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown => {
                    Some(outcome.summary.clone())
                }
                LaunchSessionOutcomeKind::Clean | LaunchSessionOutcomeKind::Stopped => None,
            })
        })
        .or_else(|| {
            fallback_message
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map(str::to_string)
        })?;

    let message = sanitize_evidence_text(
        &message,
        RedactionAudience::UserVisible,
        MAX_NOTICE_MESSAGE_BYTES,
    )
    .unwrap_or_else(|| PRIVATE_NOTICE_FALLBACK.to_string());
    let details = details
        .into_iter()
        .filter_map(|detail| {
            sanitize_evidence_text(
                &detail,
                RedactionAudience::UserVisible,
                MAX_NOTICE_DETAIL_BYTES,
            )
        })
        .take(MAX_NOTICE_DETAILS)
        .collect::<Vec<_>>();
    Some(LaunchNotice {
        message,
        detail: details.first().cloned(),
        details,
        tone: launch_notice_tone(guardian, healing, outcome),
    })
}

pub(crate) fn launch_status_snapshot(record: &LaunchSessionRecord) -> LaunchStatusEvent {
    let mut status = axial_launcher::snapshot_status(record);
    status.notice = launch_notice_from_values(
        status.guardian.as_ref(),
        status.healing.as_ref(),
        status.outcome.as_ref(),
        status.failure_detail.as_deref(),
        None,
    );
    status
}

pub(crate) fn launch_session_outcome(reason: LaunchSessionExitReason) -> LaunchSessionOutcome {
    let mut outcome = LaunchSessionOutcome::from_reason(reason);
    if reason == LaunchSessionExitReason::WatchdogKilled {
        outcome.summary = "Guardian stopped a stalled startup.".to_string();
    }
    outcome
}

fn guardian_notice_details(guardian: Option<&GuardianSummary>) -> Vec<String> {
    let Some(guardian) = guardian else {
        return Vec::new();
    };
    if !guardian.details.is_empty() {
        return guardian.details.clone();
    }
    let mut details = Vec::new();
    for intervention in &guardian.interventions {
        if !intervention.silent.unwrap_or(false) {
            push_notice_detail(&mut details, intervention.public_detail.as_deref());
        }
    }
    for guidance in &guardian.guidance {
        push_notice_detail(&mut details, Some(guidance));
    }
    details
}

fn guardian_notice_message(guardian: Option<&GuardianSummary>) -> Option<String> {
    let guardian = guardian?;
    if guardian.decision == GuardianSummaryDecision::Allowed {
        return None;
    }
    if let Some(message) = guardian
        .message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Some(message.to_string());
    }
    match guardian.decision {
        GuardianSummaryDecision::Blocked => {
            Some("Guardian blocked an unsafe launch setup.".to_string())
        }
        GuardianSummaryDecision::Warned => {
            Some("Guardian found launch settings to review.".to_string())
        }
        GuardianSummaryDecision::Intervened => {
            Some("Guardian adjusted launch settings for safety.".to_string())
        }
        GuardianSummaryDecision::Allowed => None,
    }
}

fn healing_notice_message(healing: Option<&LaunchHealingSummary>) -> Option<String> {
    let healing = healing?;
    if healing.failure_class.is_some()
        && healing.retry_count.unwrap_or_default() == 0
        && healing.fallback_applied.is_none()
    {
        if healing.failure_class.as_deref() == Some("java_runtime_mismatch") {
            return Some(
                "Launch stopped before startup because the required Java runtime was not available."
                    .to_string(),
            );
        }
        return Some(
            "Launch stopped before startup because the selected setup was not compatible."
                .to_string(),
        );
    }
    if healing.retry_count.is_some_and(|count| count > 0) {
        return Some("Launch recovered automatically with safer settings.".to_string());
    }
    if healing.fallback_applied.is_some() || !healing.warnings.is_empty() {
        return Some("Launch settings were adjusted for compatibility.".to_string());
    }
    None
}

fn healing_notice_details(healing: Option<&LaunchHealingSummary>) -> Vec<String> {
    let Some(healing) = healing else {
        return Vec::new();
    };
    let mut details = Vec::new();
    for event in &healing.events {
        push_notice_detail(&mut details, Some(healing_event_detail(event)));
    }
    for warning in &healing.warnings {
        push_notice_detail(&mut details, Some(warning));
    }
    push_notice_detail(&mut details, healing.fallback_applied.as_deref());
    if let Some(retry_count) = healing.retry_count.filter(|count| *count > 0) {
        push_notice_detail(
            &mut details,
            Some(&format!(
                "Recovered automatically after {retry_count} {}.",
                if retry_count == 1 { "retry" } else { "retries" }
            )),
        );
    }
    if let Some(failure_class) = healing.failure_class.as_deref() {
        push_notice_detail(
            &mut details,
            Some(&format!(
                "Reason: {}",
                LaunchFailureClass::from_name(failure_class)
                    .map(axial_launcher::format_failure_class)
                    .unwrap_or("startup failure")
            )),
        );
    }
    details
}

fn healing_event_detail(event: &axial_launcher::HealingEvent) -> &str {
    match event.kind {
        HealingEventKind::RuntimeBypassed => {
            "Java override was skipped and the managed runtime was used instead."
        }
        HealingEventKind::PresetDowngraded => event
            .detail
            .as_deref()
            .unwrap_or("GC preset was adjusted for compatibility."),
        HealingEventKind::FallbackApplied => event
            .detail
            .as_deref()
            .unwrap_or("Axial retried startup with safer settings."),
    }
}

fn launch_notice_tone(
    guardian: Option<&GuardianSummary>,
    healing: Option<&LaunchHealingSummary>,
    outcome: Option<&LaunchSessionOutcome>,
) -> LaunchNoticeTone {
    match guardian.map(|guardian| guardian.decision) {
        Some(GuardianSummaryDecision::Blocked) => return LaunchNoticeTone::Error,
        Some(GuardianSummaryDecision::Warned) => return LaunchNoticeTone::Warned,
        Some(GuardianSummaryDecision::Intervened) => return LaunchNoticeTone::Intervened,
        Some(GuardianSummaryDecision::Allowed) | None => {}
    }
    if outcome.is_some_and(|outcome| {
        matches!(
            outcome.kind,
            LaunchSessionOutcomeKind::Failed | LaunchSessionOutcomeKind::Unknown
        )
    }) || healing.is_some_and(|healing| healing.failure_class.is_some())
    {
        return LaunchNoticeTone::Error;
    }
    if healing.is_some_and(|healing| healing.retry_count.is_some_and(|count| count > 0)) {
        return LaunchNoticeTone::Success;
    }
    LaunchNoticeTone::Info
}

fn push_notice_detail(details: &mut Vec<String>, detail: Option<&str>) {
    let Some(detail) = detail
        .map(ensure_sentence)
        .filter(|detail| !detail.is_empty())
    else {
        return;
    };
    if !details.iter().any(|existing| existing == &detail) {
        details.push(detail);
    }
}

fn ensure_sentence(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        return String::new();
    }
    if detail.ends_with(['.', '!', '?']) {
        detail.to_string()
    } else {
        format!("{detail}.")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianInstallOutcomeSummary {
    diagnosis_id: DiagnosisId,
    decision: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    guidance: Vec<String>,
}

impl GuardianInstallOutcomeSummary {
    pub fn diagnosis_id(&self) -> DiagnosisId {
        self.diagnosis_id
    }

    pub fn decision(&self) -> &str {
        &self.decision
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn guidance(&self) -> &[String] {
        &self.guidance
    }

    pub(crate) fn retry_disabled_reason(&self) -> &str {
        self.guidance
            .first()
            .map(String::as_str)
            .or(self.detail.as_deref())
            .unwrap_or(&self.label)
    }
}

pub(crate) fn guardian_install_outcome_persistence_facts(
    outcome: &GuardianUserOutcome,
) -> Vec<String> {
    let mut facts = vec![
        format!(
            "{GUARDIAN_OUTCOME_DECISION_PREFIX}{}",
            guardian_action_persisted_id(outcome.decision)
        ),
        format!("{GUARDIAN_OUTCOME_SUMMARY_PREFIX}{}", outcome.summary),
    ];
    if let Some(detail) = outcome.details.first() {
        facts.push(format!("{GUARDIAN_OUTCOME_DETAIL_PREFIX}{detail}"));
    }
    facts
}

pub(crate) struct GuardianInstallOutcomeFactGroup<'a> {
    decision: Option<&'a str>,
    summary: Option<&'a str>,
    detail: Option<&'a str>,
    has_duplicate: bool,
}

pub(crate) fn guardian_install_outcome_fact_group<'a>(
    facts: impl IntoIterator<Item = &'a str>,
) -> Option<GuardianInstallOutcomeFactGroup<'a>> {
    let mut group = GuardianInstallOutcomeFactGroup {
        decision: None,
        summary: None,
        detail: None,
        has_duplicate: false,
    };
    let mut has_marker = false;
    for fact in facts {
        let marker = if let Some(value) = fact.strip_prefix(GUARDIAN_OUTCOME_DECISION_PREFIX) {
            Some((&mut group.decision, value))
        } else if let Some(value) = fact.strip_prefix(GUARDIAN_OUTCOME_SUMMARY_PREFIX) {
            Some((&mut group.summary, value))
        } else {
            fact.strip_prefix(GUARDIAN_OUTCOME_DETAIL_PREFIX)
                .map(|value| (&mut group.detail, value))
        };
        if let Some((slot, value)) = marker {
            has_marker = true;
            if slot.replace(value).is_some() {
                group.has_duplicate = true;
            }
        }
    }
    has_marker.then_some(group)
}

pub(crate) fn guardian_install_outcome_from_persisted_group(
    diagnosis_id: DiagnosisId,
    group: GuardianInstallOutcomeFactGroup<'_>,
) -> Option<GuardianInstallOutcomeSummary> {
    if group.has_duplicate {
        return None;
    }
    let decision = group.decision.and_then(guardian_action_from_persisted_id)?;
    let persisted_summary = group.summary?;
    let persisted_detail = group.detail;
    let canonical = author_guardian_copy(GuardianCopyRequest::install_failure_replay(
        diagnosis_id,
        decision,
    ))?;
    if persisted_summary != canonical.summary {
        return None;
    }

    let detail = match (persisted_detail, canonical.details.first()) {
        (Some(detail), Some(canonical_detail)) => Some(validated_install_detail(
            diagnosis_id,
            detail,
            canonical_detail,
        )?),
        (None, None) => None,
        _ => return None,
    };
    Some(GuardianInstallOutcomeSummary {
        diagnosis_id,
        decision: guardian_action_persisted_id(decision).to_string(),
        label: canonical.summary,
        detail,
        guidance: canonical.guidance,
    })
}

fn validated_persisted_copy_line(value: &str) -> Option<String> {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, MAX_LINE_BYTES)
        .filter(|sanitized| sanitized == value && sanitized.len() <= MAX_LINE_BYTES)
}

fn validated_install_detail(
    diagnosis_id: DiagnosisId,
    value: &str,
    canonical: &str,
) -> Option<String> {
    let value = validated_persisted_copy_line(value)?;
    if value == canonical {
        return Some(value);
    }
    match diagnosis_id {
        DiagnosisId::ManagedRuntimeUnavailableForPlatform => {
            let body = value
                .strip_prefix("Java runtime component ")?
                .strip_suffix('.')?;
            let (component, platform) = body.split_once(" is not available for ")?;
            validate_dynamic_install_token(component)?;
            validate_dynamic_install_token(platform)?;
        }
        DiagnosisId::ManagedRuntimeRosettaRequired => {
            let component = value
                .strip_prefix("Java runtime component ")?
                .strip_suffix(" needs Rosetta 2 on this Mac.")?;
            validate_dynamic_install_token(component)?;
        }
        _ => return None,
    }
    Some(value)
}

fn validate_dynamic_install_token(value: &str) -> Option<()> {
    if matches!(value, "the required runtime" | "this device") {
        return Some(());
    }
    sanitize_evidence_token(
        value,
        RedactionAudience::UserVisible,
        MAX_DYNAMIC_TOKEN_BYTES,
    )
    .filter(|sanitized| sanitized == value)
    .map(|_| ())
}

const fn guardian_action_persisted_id(action: GuardianActionKind) -> &'static str {
    match action {
        GuardianActionKind::Allow => "allow",
        GuardianActionKind::Warn => "warn",
        GuardianActionKind::Repair => "repair",
        GuardianActionKind::Retry => "retry",
        GuardianActionKind::Strip => "strip",
        GuardianActionKind::Downgrade => "downgrade",
        GuardianActionKind::Fallback => "fallback",
        GuardianActionKind::Quarantine => "quarantine",
        GuardianActionKind::AskUser => "ask_user",
        GuardianActionKind::Block => "block",
        GuardianActionKind::RecordOnly => "record_only",
    }
}

fn guardian_action_from_persisted_id(value: &str) -> Option<GuardianActionKind> {
    match value {
        "allow" => Some(GuardianActionKind::Allow),
        "warn" => Some(GuardianActionKind::Warn),
        "repair" => Some(GuardianActionKind::Repair),
        "retry" => Some(GuardianActionKind::Retry),
        "strip" => Some(GuardianActionKind::Strip),
        "downgrade" => Some(GuardianActionKind::Downgrade),
        "fallback" => Some(GuardianActionKind::Fallback),
        "quarantine" => Some(GuardianActionKind::Quarantine),
        "ask_user" => Some(GuardianActionKind::AskUser),
        "block" => Some(GuardianActionKind::Block),
        "record_only" => Some(GuardianActionKind::RecordOnly),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardianUserOutcome {
    decision: GuardianActionKind,
    phase: OperationPhase,
    summary: String,
    details: Vec<String>,
    guidance: Vec<String>,
}

impl GuardianUserOutcome {
    fn authored(
        decision: GuardianActionKind,
        phase: OperationPhase,
        summary: String,
        details: Vec<String>,
        guidance: Vec<String>,
    ) -> Self {
        Self {
            decision,
            phase,
            summary,
            details,
            guidance,
        }
    }

    pub fn decision(&self) -> GuardianActionKind {
        self.decision
    }

    pub fn phase(&self) -> OperationPhase {
        self.phase
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn details(&self) -> &[String] {
        &self.details
    }

    pub fn guidance(&self) -> &[String] {
        &self.guidance
    }
}

pub(crate) fn guardian_directive_description(directive: &GuardianDirective) -> String {
    let rendered = match directive {
        GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::Preflight,
        } => "Guardian will use managed Java for this launch.".to_string(),
        GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::PrepareFailure,
        } => "Guardian switched to managed Java before launch".to_string(),
        GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::StartupRecovery,
        } => "Automatic retry: switched to managed Java after runtime mismatch".to_string(),
        GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::Preflight,
        } => "Guardian will remove incompatible explicit JVM args for this launch.".to_string(),
        GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::PrepareFailure,
        } => "Guardian removed incompatible explicit JVM args before launch".to_string(),
        GuardianDirective::DowngradeJvmPreset {
            preset,
            reason: GuardianPresetDowngradeReason::Compatibility { requested_preset },
        } => format!(
            "Guardian downgraded JVM preset from \"{requested_preset}\" to \"{preset}\" before launch"
        ),
        GuardianDirective::DowngradeJvmPreset {
            preset,
            reason: GuardianPresetDowngradeReason::StartupRecovery,
        } => {
            format!("Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure")
        }
        GuardianDirective::DisableCustomGc => {
            "Automatic retry: disabled custom GC flags after startup failure".to_string()
        }
    };
    checked_rendered_line(rendered)
}

fn guardian_directive_recovery_label(directive: &GuardianDirective) -> &'static str {
    match directive {
        GuardianDirective::UseManagedJava { .. } => "managed Java recovery",
        GuardianDirective::StripJvmArgs { .. } => "explicit JVM argument recovery",
        GuardianDirective::DowngradeJvmPreset { .. } => "JVM preset recovery",
        GuardianDirective::DisableCustomGc => "custom GC flag recovery",
    }
}

pub(crate) fn guardian_failed_launch_recovery_log(directive: &GuardianDirective) -> String {
    checked_rendered_line(format!(
        "Guardian recorded failed launch self-healing for {}.",
        guardian_directive_recovery_label(directive)
    ))
}

pub(crate) struct GuardianRuntimeRepairCopy {
    status: GuardianRepairStatus,
    user_outcome: GuardianUserOutcome,
}

#[derive(Clone)]
pub(crate) enum GuardianLaunchAdmission {
    Preflight {
        user_outcome: GuardianUserOutcome,
        safety: super::SafetyOutcome,
    },
    RuntimeRepairBlock {
        user_outcome: GuardianUserOutcome,
        safety: super::SafetyOutcome,
    },
}

impl GuardianLaunchAdmission {
    pub(crate) fn preflight(current: &GuardianPreflightOutcome) -> Self {
        Self::Preflight {
            user_outcome: current.user_outcome.clone(),
            safety: current.safety.clone(),
        }
    }

    pub(crate) fn user_outcome(&self) -> &GuardianUserOutcome {
        match self {
            Self::Preflight { user_outcome, .. }
            | Self::RuntimeRepairBlock { user_outcome, .. } => user_outcome,
        }
    }

    pub(crate) fn safety(&self) -> &super::SafetyOutcome {
        match self {
            Self::Preflight { safety, .. } | Self::RuntimeRepairBlock { safety, .. } => safety,
        }
    }

    fn public_lines(&self) -> Vec<String> {
        let outcome = self.user_outcome();
        let mut lines = Vec::new();
        for value in outcome.details.iter().chain(outcome.guidance.iter()) {
            push_unique_copy_line(&mut lines, value);
        }
        lines
    }
}

impl GuardianRuntimeRepairCopy {
    pub(crate) fn author(
        diagnosis_id: Option<DiagnosisId>,
        status: GuardianRepairStatus,
    ) -> Option<Self> {
        let user_outcome =
            author_guardian_copy(GuardianCopyRequest::runtime_repair(diagnosis_id, status))?;
        Some(Self {
            status,
            user_outcome,
        })
    }

    pub(crate) fn guardian_summary(&self, current: &GuardianSummary) -> GuardianSummary {
        match self.status {
            GuardianRepairStatus::Repaired => {
                repaired_guardian_summary(current, &self.user_outcome)
            }
            GuardianRepairStatus::Blocked | GuardianRepairStatus::Failed => {
                blocked_repair_guardian_summary(current, &self.user_outcome)
            }
        }
    }

    pub(crate) fn blocked_admission(
        &self,
        current: &GuardianPreflightOutcome,
    ) -> Option<GuardianLaunchAdmission> {
        if self.status == GuardianRepairStatus::Repaired {
            return None;
        }
        let user_outcome = GuardianUserOutcome::authored(
            GuardianActionKind::Block,
            current.user_outcome.phase,
            self.user_outcome.summary.clone(),
            self.user_outcome.details.clone(),
            self.user_outcome.guidance.clone(),
        );
        Some(GuardianLaunchAdmission::RuntimeRepairBlock {
            safety: super::SafetyOutcome {
                decision: GuardianActionKind::Block,
                summary: user_outcome.summary.clone(),
                detail: user_outcome.details.first().cloned(),
                diagnoses: current.safety.diagnoses.clone(),
            },
            user_outcome,
        })
    }
}

fn repaired_guardian_summary(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    let mut summary = current.clone();
    let previous_details = summary.details.clone();
    let previous_guidance = summary.guidance.clone();
    summary.decision = GuardianSummaryDecision::Intervened;
    summary.message = Some(outcome.summary.clone());
    summary.details.clear();
    for detail in &outcome.details {
        push_unique_copy_line(&mut summary.details, detail);
    }
    for detail in previous_details {
        push_unique_copy_line(&mut summary.details, &detail);
    }
    for detail in &previous_guidance {
        push_unique_copy_line(&mut summary.details, detail);
    }
    summary.guidance.clear();
    for detail in previous_guidance {
        push_unique_copy_line(&mut summary.guidance, &detail);
    }
    summary
}

fn blocked_repair_guardian_summary(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    let mut details = Vec::new();
    for detail in outcome
        .details
        .iter()
        .chain(current.details.iter())
        .chain(current.guidance.iter())
        .chain(outcome.guidance.iter())
    {
        push_unique_copy_line(&mut details, detail);
    }
    let mut guidance = Vec::new();
    for detail in current.guidance.iter().chain(outcome.guidance.iter()) {
        push_unique_copy_line(&mut guidance, detail);
    }
    GuardianSummary {
        mode: current.mode,
        decision: GuardianSummaryDecision::Blocked,
        message: Some(outcome.summary.clone()),
        details,
        guidance,
        interventions: current.interventions.clone(),
    }
}

fn push_unique_copy_line(lines: &mut Vec<String>, value: &str) {
    if lines.len() >= MAX_COLLECTION_LINES {
        return;
    }
    let Some(value) = sanitize_evidence_text(value, RedactionAudience::UserVisible, MAX_LINE_BYTES)
        .filter(|value| value.len() <= MAX_LINE_BYTES)
    else {
        return;
    };
    if !lines.iter().any(|line| line == &value) {
        lines.push(value);
    }
}

pub(crate) fn guardian_summary_with_blocked_outcome(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    let mut projected = current.clone();
    let existing_guidance = projected.guidance.clone();
    let mut guidance = Vec::new();
    for detail in existing_guidance.iter().chain(outcome.guidance.iter()) {
        push_unique_copy_line(&mut guidance, detail);
    }
    let mut details = Vec::new();
    for detail in outcome
        .details
        .iter()
        .chain(existing_guidance.iter())
        .chain(outcome.guidance.iter())
    {
        push_unique_copy_line(&mut details, detail);
    }
    projected.decision = GuardianSummaryDecision::Blocked;
    projected.message = Some(outcome.summary.clone());
    projected.details = details;
    projected.guidance = guidance;
    projected
}

pub(crate) fn guardian_summary_with_suppressed_outcome(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    let mut guidance = Vec::new();
    for detail in current.guidance.iter().chain(outcome.guidance.iter()) {
        push_unique_copy_line(&mut guidance, detail);
    }
    let reason = outcome
        .details
        .first()
        .cloned()
        .unwrap_or_else(|| outcome.summary.clone());
    let mut details = Vec::new();
    push_unique_copy_line(&mut details, &reason);
    for detail in &current.details {
        push_unique_copy_line(&mut details, detail);
    }
    for detail in &guidance {
        push_unique_copy_line(&mut details, detail);
    }
    GuardianSummary {
        mode: current.mode,
        decision: GuardianSummaryDecision::Blocked,
        message: Some("Guardian blocked an unsafe launch setup.".to_string()),
        details,
        guidance,
        interventions: current.interventions.clone(),
    }
}

pub(crate) fn guardian_summary_with_observed_outcome(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    if outcome.decision == GuardianActionKind::Block {
        return guardian_summary_with_blocked_outcome(current, outcome);
    }
    let mut projected = current.clone();
    projected.decision = GuardianSummaryDecision::Warned;
    projected.message = Some(outcome.summary.clone());
    let mut details = Vec::new();
    for detail in current.details.iter().chain(outcome.details.iter()) {
        push_unique_copy_line(&mut details, detail);
    }
    let mut guidance = Vec::new();
    for detail in current.guidance.iter().chain(outcome.guidance.iter()) {
        push_unique_copy_line(&mut guidance, detail);
    }
    projected.details = details;
    projected.guidance = guidance;
    projected
}

pub(crate) fn guardian_summary_with_artifact_repair_outcome(
    current: &GuardianSummary,
    outcome: &GuardianUserOutcome,
) -> GuardianSummary {
    if outcome.decision == GuardianActionKind::Repair {
        repaired_guardian_summary(current, outcome)
    } else {
        blocked_repair_guardian_summary(current, outcome)
    }
}

pub(crate) fn guardian_summary_from_admission(
    mode: LauncherGuardianMode,
    admission: &GuardianLaunchAdmission,
) -> GuardianSummary {
    let outcome = admission.user_outcome();
    let public_lines = admission.public_lines();
    let decision = match outcome.decision {
        GuardianActionKind::Allow | GuardianActionKind::RecordOnly => {
            GuardianSummaryDecision::Allowed
        }
        GuardianActionKind::Warn => GuardianSummaryDecision::Warned,
        GuardianActionKind::Block => GuardianSummaryDecision::Blocked,
        GuardianActionKind::AskUser => {
            unreachable!("preflight boundary must resolve confirmation before launch conversion")
        }
        GuardianActionKind::Repair
        | GuardianActionKind::Retry
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Fallback
        | GuardianActionKind::Quarantine => GuardianSummaryDecision::Intervened,
    };
    GuardianSummary {
        mode,
        decision,
        message: Some(outcome.summary.clone()),
        details: public_lines.clone(),
        guidance: public_lines,
        interventions: Vec::new(),
    }
}

pub(crate) fn guardian_summary_with_intervention(
    current: &GuardianSummary,
    directive: &GuardianDirective,
    silent: bool,
) -> GuardianSummary {
    let mut details = Vec::new();
    for intervention in &current.interventions {
        if !intervention.silent.unwrap_or(false)
            && let Some(detail) = &intervention.public_detail
        {
            push_unique_copy_line(&mut details, detail);
        }
    }
    let public_detail = guardian_intervention_public_detail(directive);
    if !silent {
        push_unique_copy_line(&mut details, &public_detail);
    }
    for detail in &current.guidance {
        push_unique_copy_line(&mut details, detail);
    }

    let mut guidance = Vec::new();
    for detail in &current.guidance {
        push_unique_copy_line(&mut guidance, detail);
    }
    let mut interventions = current.interventions.clone();
    interventions.push(GuardianIntervention {
        kind: guardian_intervention_kind(directive),
        detail: Some(guardian_directive_description(directive)),
        public_detail: Some(public_detail),
        silent: Some(silent),
    });
    GuardianSummary {
        mode: current.mode,
        decision: GuardianSummaryDecision::Intervened,
        message: Some("Guardian adjusted launch settings for safety.".to_string()),
        details,
        guidance,
        interventions,
    }
}

fn guardian_intervention_kind(directive: &GuardianDirective) -> GuardianInterventionKind {
    match directive {
        GuardianDirective::UseManagedJava { .. } => GuardianInterventionKind::SwitchManagedRuntime,
        GuardianDirective::StripJvmArgs { .. } => GuardianInterventionKind::StripJvmArgs,
        GuardianDirective::DowngradeJvmPreset { .. } => GuardianInterventionKind::DowngradePreset,
        GuardianDirective::DisableCustomGc => GuardianInterventionKind::DisableCustomGc,
    }
}

fn guardian_intervention_public_detail(directive: &GuardianDirective) -> String {
    match directive {
        GuardianDirective::UseManagedJava { .. } => {
            "Guardian used the managed Java runtime for this launch.".to_string()
        }
        GuardianDirective::StripJvmArgs { .. } => {
            "Explicit JVM args were removed before launch because they were incompatible."
                .to_string()
        }
        GuardianDirective::DowngradeJvmPreset {
            preset,
            reason: GuardianPresetDowngradeReason::Compatibility { requested_preset },
        } => format!(
            "JVM preset changed from {} to {} for compatibility.",
            guardian_preset_display_name(requested_preset),
            guardian_preset_display_name(preset)
        ),
        GuardianDirective::DowngradeJvmPreset {
            preset,
            reason: GuardianPresetDowngradeReason::StartupRecovery,
        } => format!(
            "JVM preset changed to {} for compatibility.",
            guardian_preset_display_name(preset)
        ),
        GuardianDirective::DisableCustomGc => {
            "Custom GC flags were disabled for compatibility.".to_string()
        }
    }
}

fn guardian_preset_display_name(preset: &GuardianPresetValue) -> String {
    match preset.as_str() {
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
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
                })
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
pub(crate) fn guardian_user_outcome_for_test(
    decision: GuardianActionKind,
    phase: OperationPhase,
    summary: &str,
    details: &[&str],
    guidance: &[&str],
) -> GuardianUserOutcome {
    let mut bounded_details = Vec::new();
    for detail in details {
        push_unique_copy_line(&mut bounded_details, detail);
    }
    let mut bounded_guidance = Vec::new();
    for detail in guidance {
        push_unique_copy_line(&mut bounded_guidance, detail);
    }
    GuardianUserOutcome::authored(
        decision,
        phase,
        trusted_line_for_test(summary, MAX_SUMMARY_BYTES),
        bounded_details,
        bounded_guidance,
    )
}

#[cfg(test)]
fn trusted_line_for_test(value: &str, max_bytes: usize) -> String {
    assert!(!value.is_empty() && value.len() <= max_bytes);
    value.to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct GuardianCopyRequest<'a> {
    diagnosis_id: Option<DiagnosisId>,
    context: GuardianCopyContext<'a>,
}

#[derive(Clone, Debug)]
enum GuardianCopyContext<'a> {
    RuntimeRepair {
        status: GuardianRepairStatus,
    },
    ArtifactRepair {
        status: GuardianArtifactRepairStatus,
    },
    InstallFailure {
        decision: GuardianActionKind,
        dynamics: InstallCopyDynamics<'a>,
    },
    PerformanceRejection {
        rejection: GuardianPerformanceSupervisionRejection,
        phase: OperationPhase,
    },
    PersistedStateLoad {
        decision: GuardianActionKind,
    },
    Preflight {
        authored_decision: GuardianActionKind,
        effective_decision: GuardianActionKind,
        phase: OperationPhase,
        diagnoses: Vec<DiagnosisId>,
        history: Vec<PreflightHistory>,
    },
    PrepareFailure {
        decision: GuardianActionKind,
        failure_class: LaunchFailureClass,
        public_error: Option<String>,
        explicit_java_override_present: bool,
        explicit_jvm_args_present: bool,
        directive: Option<GuardianDirective>,
    },
    StartupFailure {
        decision: GuardianActionKind,
        failure_class: LaunchFailureClass,
        stalled: bool,
        first_suspected_mod: Option<String>,
        explicit_java_override_present: bool,
        explicit_jvm_args_present: bool,
        explicit_jvm_preset_present: bool,
        user_mod_set_drift: bool,
        directive: Option<GuardianDirective>,
    },
    ObservedLaunchFailure {
        failure_class: LaunchFailureClass,
        observed_phase: GuardianObservedLaunchFailurePhase,
        first_suspected_mod: Option<String>,
    },
    LaunchRecoverySuppressed {
        directive: GuardianDirective,
    },
}

#[derive(Clone, Copy, Debug)]
enum InstallCopyDynamics<'a> {
    None,
    RuntimeUnavailable {
        component: Option<&'a str>,
        platform: Option<&'a str>,
    },
    Rosetta {
        component: Option<&'a str>,
    },
}

impl<'a> GuardianCopyRequest<'a> {
    pub(crate) fn runtime_repair(
        diagnosis_id: Option<DiagnosisId>,
        status: GuardianRepairStatus,
    ) -> Self {
        Self {
            diagnosis_id,
            context: GuardianCopyContext::RuntimeRepair { status },
        }
    }

    pub(crate) fn artifact_repair(
        diagnosis_id: DiagnosisId,
        status: GuardianArtifactRepairStatus,
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::ArtifactRepair { status },
        }
    }

    pub(crate) fn install_failure(
        diagnosis_id: DiagnosisId,
        decision: GuardianActionKind,
        evidence: &'a [GuardianInstallArtifactFailureEvidence],
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::InstallFailure {
                decision,
                dynamics: install_copy_dynamics(diagnosis_id, evidence),
            },
        }
    }

    fn install_failure_replay(diagnosis_id: DiagnosisId, decision: GuardianActionKind) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::InstallFailure {
                decision,
                dynamics: InstallCopyDynamics::None,
            },
        }
    }

    pub(crate) fn performance_rejection(
        rejection: GuardianPerformanceSupervisionRejection,
        phase: OperationPhase,
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::PerformanceRejection { rejection, phase },
        }
    }

    pub(crate) fn persisted_state_load(
        diagnosis_id: DiagnosisId,
        decision: GuardianActionKind,
    ) -> Self {
        Self {
            diagnosis_id: Some(diagnosis_id),
            context: GuardianCopyContext::PersistedStateLoad { decision },
        }
    }

    pub(crate) fn preflight(
        authored_decision: GuardianActionKind,
        effective_decision: GuardianActionKind,
        phase: OperationPhase,
        diagnoses: &[DiagnosisId],
        facts: &[GuardianFact],
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::Preflight {
                authored_decision,
                effective_decision,
                phase,
                diagnoses: diagnoses.to_vec(),
                history: preflight_history(facts),
            },
        }
    }

    pub(crate) fn prepare_failure(
        decision: GuardianActionKind,
        failure_class: LaunchFailureClass,
        public_error: &str,
        explicit_java_override_present: bool,
        explicit_jvm_args_present: bool,
        directive: Option<&GuardianDirective>,
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::PrepareFailure {
                decision,
                failure_class,
                public_error: sanitize_evidence_text(
                    public_error,
                    RedactionAudience::UserVisible,
                    MAX_LINE_BYTES,
                )
                .filter(|public_error| public_error.len() <= MAX_LINE_BYTES),
                explicit_java_override_present,
                explicit_jvm_args_present,
                directive: directive.cloned(),
            },
        }
    }

    pub(crate) fn startup_failure(
        decision: GuardianActionKind,
        request: &GuardianStartupFailureRequest<'_>,
        directive: Option<&GuardianDirective>,
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::StartupFailure {
                decision,
                failure_class: match request.observation {
                    GuardianStartupFailureObservation::Stalled => {
                        LaunchFailureClass::StartupStalled
                    }
                    GuardianStartupFailureObservation::Exited { failure_class } => failure_class,
                },
                stalled: matches!(
                    request.observation,
                    GuardianStartupFailureObservation::Stalled
                ),
                first_suspected_mod: first_suspected_mod(request.crash_evidence),
                explicit_java_override_present: request.explicit_java_override_present,
                explicit_jvm_args_present: request.explicit_jvm_args_present,
                explicit_jvm_preset_present: request.explicit_jvm_preset_present,
                user_mod_set_drift: request
                    .integrity_facts
                    .iter()
                    .any(|fact| fact.id == GuardianFactId::UserModSetDrift),
                directive: directive.cloned(),
            },
        }
    }

    pub(crate) fn observed_launch_failure(
        failure_class: LaunchFailureClass,
        crash_evidence: Option<&CrashEvidence>,
        observed_phase: GuardianObservedLaunchFailurePhase,
    ) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::ObservedLaunchFailure {
                failure_class,
                observed_phase,
                first_suspected_mod: first_suspected_mod(crash_evidence),
            },
        }
    }

    pub(crate) fn launch_recovery_suppressed(directive: &GuardianDirective) -> Self {
        Self {
            diagnosis_id: None,
            context: GuardianCopyContext::LaunchRecoverySuppressed {
                directive: directive.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreflightHistory {
    StartupFailure {
        class: PreflightCrashClass,
        window: PreflightOccurrenceWindow,
        oom_budget: Option<PreflightOomBudget>,
    },
    RepairFailed(PreflightRecoveryKind),
    Suppressed(PreflightSuppressionTime),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightCrashClass {
    OutOfMemory,
    GraphicsDriverCrash,
    MissingDependency,
    ModTransformationFailure,
    ModAttributedCrash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightOccurrenceWindow {
    Today(u32),
    Total { count: u32, latest_today: bool },
    Recent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightOomBudget {
    Concrete { current_mb: u32, suggested_mb: u32 },
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreflightRecoveryKind {
    JavaRuntime,
    JvmArgs,
    JvmPreset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreflightSuppressionTime {
    hour: u32,
    minute: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyContextKey {
    RuntimeRepaired,
    RuntimeBlocked,
    RuntimeFailed,
    ArtifactRepaired,
    ArtifactBlocked,
    ArtifactFailed,
    InstallFailure,
    PerformanceUnsafeOwnership,
    PerformanceMissingJournal,
    PerformanceUnsafePublicBoundary,
    PerformanceGuardianBlocked,
    PerformanceFallbackUnavailable,
    PerformanceRollbackUnavailable,
    PersistedStateLoad,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyRuleKey {
    diagnosis_id: Option<DiagnosisId>,
    decision: GuardianActionKind,
    context: CopyContextKey,
}

#[derive(Clone, Copy)]
enum CopyPhase {
    Fixed(OperationPhase),
    PerformanceContext,
}

#[derive(Clone, Copy)]
enum CopyLine {
    Static(&'static str),
    RuntimeUnavailableDetail,
    RuntimeRosettaDetail,
}

#[derive(Clone, Copy)]
struct GuardianCopyRule {
    key: CopyRuleKey,
    phase: CopyPhase,
    summary: &'static str,
    details: &'static [CopyLine],
    guidance: &'static [CopyLine],
}

const fn key(
    diagnosis_id: Option<DiagnosisId>,
    decision: GuardianActionKind,
    context: CopyContextKey,
) -> CopyRuleKey {
    CopyRuleKey {
        diagnosis_id,
        decision,
        context,
    }
}

const fn fixed_rule(
    key: CopyRuleKey,
    phase: OperationPhase,
    summary: &'static str,
    details: &'static [CopyLine],
    guidance: &'static [CopyLine],
) -> GuardianCopyRule {
    GuardianCopyRule {
        key,
        phase: CopyPhase::Fixed(phase),
        summary,
        details,
        guidance,
    }
}

const PERFORMANCE_SUMMARY: &str = "performance update was blocked by Guardian safety supervision";

const GUARDIAN_COPY_RULES: &[GuardianCopyRule] = &[
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Repair,
            CopyContextKey::RuntimeRepaired,
        ),
        OperationPhase::Repairing,
        "Guardian repaired launch state before launch.",
        &[CopyLine::Static(
            "Guardian repaired the managed Java runtime before launch.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::RuntimeBlocked,
        ),
        OperationPhase::Repairing,
        "Guardian blocked launch preflight.",
        &[CopyLine::Static(
            "Guardian blocked managed Java runtime repair because it was not safe to apply.",
        )],
        &[CopyLine::Static(
            "Reinstall or repair the affected version/runtime before launching again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::RuntimeFailed,
        ),
        OperationPhase::Repairing,
        "Guardian blocked launch preflight.",
        &[CopyLine::Static(
            "Guardian could not repair the managed Java runtime automatically.",
        )],
        &[CopyLine::Static(
            "Reinstall or repair the affected version/runtime before launching again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Repair,
            CopyContextKey::ArtifactRepaired,
        ),
        OperationPhase::Repairing,
        "Guardian repaired a launcher-managed artifact.",
        &[CopyLine::Static(
            "The launcher can continue from the repaired state.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::ArtifactBlocked,
        ),
        OperationPhase::Repairing,
        "Guardian blocked automatic artifact repair because it was unsafe.",
        &[CopyLine::Static(
            "The launcher did not mutate files that were not proven launcher-managed.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::ArtifactFailed,
        ),
        OperationPhase::Repairing,
        "Guardian could not repair the launcher-managed artifact.",
        &[CopyLine::Static(
            "Check connection and storage permissions before trying again.",
        )],
        &[],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::LauncherManagedArtifactCorrupt),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian stopped install after launcher-managed artifact verification failed.",
        &[CopyLine::Static(
            "The failed attempt ended without automatically mutating or retrying the artifact.",
        )],
        &[CopyLine::Static(
            "Retry the install to start a new launcher-managed attempt.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::DownloadUnavailable),
            GuardianActionKind::Retry,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian classified the install download failure as retryable.",
        &[CopyLine::Static(
            "The install stopped because a provider or network download was unavailable or interrupted.",
        )],
        &[CopyLine::Static(
            "Retry the install after checking connection and storage availability.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::DownloadUnavailable),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian paused install retry after repeated provider failure.",
        &[CopyLine::Static(
            "The install stopped because the same provider or network download failure repeated within the retry cooldown.",
        )],
        &[CopyLine::Static(
            "Wait a few minutes, then retry after checking connection and storage availability.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallArtifactMetadataInvalid),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because provider metadata could not be trusted.",
        &[CopyLine::Static(
            "The install did not continue with invalid provider metadata.",
        )],
        &[CopyLine::Static(
            "Retry later or choose another version source.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallDependencyFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked loader install because the required base install failed.",
        &[CopyLine::Static(
            "The loader install did not continue after the base Minecraft install failed.",
        )],
        &[CopyLine::Static(
            "Retry the base version install, then retry the loader install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallExecutionFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Installing,
        "Guardian blocked loader install because installation could not complete.",
        &[CopyLine::Static(
            "A launcher-managed loader installation step failed before verification completed.",
        )],
        &[CopyLine::Static(
            "Retry the loader install. If it repeats, restart Axial before trying again.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::InstallProcessorFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Installing,
        "Guardian blocked loader install because an installer processor failed.",
        &[CopyLine::Static(
            "A loader-supplied processor did not complete successfully.",
        )],
        &[CopyLine::Static(
            "Retry the loader install or choose another loader build.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeUnavailableForPlatform),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "This Minecraft version needs a Java runtime that is not available for this device.",
        &[CopyLine::RuntimeUnavailableDetail],
        &[CopyLine::Static(
            "This version cannot be installed on this device.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ManagedRuntimeRosettaRequired),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "This Minecraft version needs Rosetta 2 on Apple Silicon Macs.",
        &[CopyLine::RuntimeRosettaDetail],
        &[CopyLine::Static(
            "Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then retry.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::FilesystemLocked),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because a launcher-managed file is in use.",
        &[CopyLine::Static(
            "The install did not continue after the filesystem reported a locked file.",
        )],
        &[CopyLine::Static(
            "Close apps that may be using launcher files, then retry the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::FilesystemPermissionDenied),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because Axial could not write launcher-managed files safely.",
        &[CopyLine::Static(
            "The install did not mutate files after the filesystem refused the operation.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and retry the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::TempFileWriteFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because temporary download state could not be written safely.",
        &[CopyLine::Static(
            "The install did not continue after temporary download state could not be written safely.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and disk availability before retrying the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::AtomicPromotionFailed),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install because verified download data could not be promoted safely.",
        &[CopyLine::Static(
            "The install did not replace launcher-managed files after atomic promotion failed.",
        )],
        &[CopyLine::Static(
            "Check app data permissions and retry the install.",
        )],
    ),
    fixed_rule(
        key(
            Some(DiagnosisId::ArtifactOwnershipUnsafe),
            GuardianActionKind::Block,
            CopyContextKey::InstallFailure,
        ),
        OperationPhase::Downloading,
        "Guardian blocked install to protect user-owned or unknown files.",
        &[CopyLine::Static(
            "The install did not automatically mutate a target whose ownership was unsafe.",
        )],
        &[CopyLine::Static(
            "Move the affected files or choose a launcher-managed library location before retrying.",
        )],
    ),
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceUnsafeOwnership,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceMissingJournal,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceUnsafePublicBoundary,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceGuardianBlocked,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceFallbackUnavailable,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    GuardianCopyRule {
        key: key(
            None,
            GuardianActionKind::Block,
            CopyContextKey::PerformanceRollbackUnavailable,
        ),
        phase: CopyPhase::PerformanceContext,
        summary: PERFORMANCE_SUMMARY,
        details: &[],
        guidance: &[],
    },
    fixed_rule(
        key(
            Some(DiagnosisId::PersistedStateSchemaInvalid),
            GuardianActionKind::Warn,
            CopyContextKey::PersistedStateLoad,
        ),
        OperationPhase::Startup,
        "Guardian kept Axial running after persisted operation state could not be trusted.",
        &[CopyLine::Static(
            "Some restart-resume records were ignored instead of resuming unsafe work.",
        )],
        &[CopyLine::Static(
            "Retry the affected performance or benchmark operation if it is still needed.",
        )],
    ),
];

#[derive(Clone, Copy)]
struct PreflightSummaryRule {
    decision: GuardianActionKind,
    summary: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreflightCopyCoordinate {
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
}

#[derive(Clone, Copy)]
struct PreflightDiagnosisCopyRule {
    coordinate: PreflightCopyCoordinate,
    detail: Option<&'static str>,
    guidance: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct PreflightInvariantDiagnosisCopyRule {
    diagnosis_id: DiagnosisId,
    decisions: &'static [GuardianActionKind],
    detail: Option<&'static str>,
    guidance: Option<&'static str>,
}

const fn preflight_summary_rule(
    decision: GuardianActionKind,
    summary: &'static str,
) -> PreflightSummaryRule {
    PreflightSummaryRule { decision, summary }
}

const fn preflight_diagnosis_rule(
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
    detail: Option<&'static str>,
    guidance: Option<&'static str>,
) -> PreflightDiagnosisCopyRule {
    PreflightDiagnosisCopyRule {
        coordinate: PreflightCopyCoordinate {
            diagnosis_id,
            decision,
        },
        detail,
        guidance,
    }
}

const fn preflight_invariant_diagnosis_rule(
    diagnosis_id: DiagnosisId,
    decisions: &'static [GuardianActionKind],
    detail: Option<&'static str>,
    guidance: Option<&'static str>,
) -> PreflightInvariantDiagnosisCopyRule {
    PreflightInvariantDiagnosisCopyRule {
        diagnosis_id,
        decisions,
        detail,
        guidance,
    }
}

const PREFLIGHT_SUMMARY_RULES: &[PreflightSummaryRule] = &[
    preflight_summary_rule(
        GuardianActionKind::RecordOnly,
        "Guardian recorded launch preflight readiness.",
    ),
    preflight_summary_rule(
        GuardianActionKind::Warn,
        "Guardian found launch preflight warnings.",
    ),
    preflight_summary_rule(
        GuardianActionKind::AskUser,
        "Guardian needs confirmation before launch.",
    ),
    preflight_summary_rule(
        GuardianActionKind::Block,
        "Guardian blocked launch preflight.",
    ),
    preflight_summary_rule(
        GuardianActionKind::Fallback,
        "Guardian adjusted launch preflight.",
    ),
    preflight_summary_rule(
        GuardianActionKind::Strip,
        "Guardian adjusted launch preflight.",
    ),
    preflight_summary_rule(
        GuardianActionKind::Repair,
        "Guardian selected a guarded launch preflight action.",
    ),
];

const INSTALL_REPAIR_GUIDANCE: &str =
    "Install or repair the affected version before launching again.";
const CUSTOM_MODE_GUIDANCE: &str =
    "Switch Guardian back to Managed if you want Axial to adjust unsafe choices.";

const PREFLIGHT_DIAGNOSIS_RULES: &[PreflightDiagnosisCopyRule] = &[
    preflight_diagnosis_rule(
        DiagnosisId::UnknownFailure(OperationPhase::Validating),
        GuardianActionKind::RecordOnly,
        None,
        None,
    ),
    preflight_diagnosis_rule(
        DiagnosisId::UnknownFailure(OperationPhase::Validating),
        GuardianActionKind::Block,
        Some("Guardian blocked launch because preflight readiness failed."),
        None,
    ),
    preflight_diagnosis_rule(
        DiagnosisId::UnknownFailure(OperationPhase::Validating),
        GuardianActionKind::Warn,
        None,
        None,
    ),
    preflight_diagnosis_rule(
        DiagnosisId::ManagedRuntimeCorrupt,
        GuardianActionKind::Repair,
        None,
        None,
    ),
    preflight_diagnosis_rule(
        DiagnosisId::ManagedRuntimeCorrupt,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because preflight readiness failed."),
        None,
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaOverrideUnavailable,
        GuardianActionKind::Fallback,
        Some(
            "Guardian will ignore the unavailable Java override and use managed Java for this launch.",
        ),
        Some(
            "Update or remove the bad Java override after launch if you want to use Custom Java again.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaOverrideUnavailable,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because the selected Java override is unavailable."),
        Some(
            "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaOverrideUnavailable,
        GuardianActionKind::AskUser,
        Some("Guardian needs confirmation before changing the selected Java override."),
        Some("Confirm managed Java for this launch or choose a valid Java runtime."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaOverrideUnavailable,
        GuardianActionKind::Warn,
        Some(
            "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
        ),
        Some(
            "Guardian detected an unavailable Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaProbeFailed,
        GuardianActionKind::Fallback,
        Some(
            "Guardian will ignore the Java override that failed probing and use managed Java for this launch.",
        ),
        Some(
            "Update or remove the Java override after launch if you want to use Custom Java again.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaProbeFailed,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because the selected Java override could not be probed."),
        Some("Use a Java runtime that can run `java -version`, or switch back to Managed Java."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaProbeFailed,
        GuardianActionKind::Strip,
        Some(
            "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
        ),
        Some("Use a Java runtime that can run `java -version`, or switch back to Managed Java."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaProbeFailed,
        GuardianActionKind::Warn,
        Some(
            "Guardian could not verify the selected Java override. Use a valid Java runtime or switch back to Managed Java before relying on this launch.",
        ),
        Some("Use a Java runtime that can run `java -version`, or switch back to Managed Java."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaRuntimeMajorMismatch,
        GuardianActionKind::Fallback,
        Some(
            "Guardian will ignore the incompatible Java override and use managed Java for this launch.",
        ),
        Some(
            "Choose a Java runtime matching this Minecraft version before re-enabling the override.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaRuntimeMajorMismatch,
        GuardianActionKind::Block,
        Some(
            "Guardian blocked launch because the selected Java override has the wrong Java version.",
        ),
        Some("Choose a Java runtime matching this Minecraft version requirement."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaRuntimeUpdateTooOld,
        GuardianActionKind::Fallback,
        Some(
            "Guardian will ignore the outdated Java override and use managed Java for this launch.",
        ),
        Some("Use Java 8u312 or newer before re-enabling this override."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JavaRuntimeUpdateTooOld,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because the selected Java 8 override is too old."),
        Some("Use Java 8u312 or newer for this legacy launch."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgsMalformed,
        GuardianActionKind::Strip,
        Some("Guardian removed malformed explicit JVM args for this launch."),
        Some("Fix the saved JVM args before re-enabling them."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgsMalformed,
        GuardianActionKind::Block,
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgsMalformed,
        GuardianActionKind::Warn,
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsupported,
        GuardianActionKind::Strip,
        Some("Guardian removed unsupported explicit JVM args for this launch."),
        Some("Use JVM flags supported by the selected Java runtime before re-enabling them."),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsupported,
        GuardianActionKind::Block,
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsupported,
        GuardianActionKind::Warn,
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsafeOverride,
        GuardianActionKind::Strip,
        Some(
            "Guardian removed explicit JVM args that override launcher-owned settings for this launch.",
        ),
        Some(
            "Remove memory, classpath, native-path, or agent overrides from saved JVM args before re-enabling them.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsafeOverride,
        GuardianActionKind::Block,
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::JvmArgUnsafeOverride,
        GuardianActionKind::Warn,
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::InstalledVersionMetadataMissing,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because installed version metadata is missing."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::ParentVersionMetadataMissing,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because parent version metadata is missing."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::InstallIncomplete,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because the install is incomplete."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::ClientJarMissing,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because client game files are missing."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::LibrariesMissing,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because required libraries are missing."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::AssetIndexMissing,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because the asset index is missing."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::LauncherManagedArtifactCorrupt,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because launcher-managed game files are corrupt."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
    preflight_diagnosis_rule(
        DiagnosisId::LauncherManagedArtifactSignatureCorrupt,
        GuardianActionKind::Block,
        Some("Guardian blocked launch because launcher-managed jar signatures are inconsistent."),
        Some(INSTALL_REPAIR_GUIDANCE),
    ),
];

const PREFLIGHT_SUPPORTING_VERDICTS: &[GuardianActionKind] = &[
    GuardianActionKind::Warn,
    GuardianActionKind::Block,
    GuardianActionKind::Fallback,
    GuardianActionKind::Strip,
    GuardianActionKind::AskUser,
];
const PREFLIGHT_CUSTOM_VERDICTS: &[GuardianActionKind] = &[
    GuardianActionKind::Warn,
    GuardianActionKind::Block,
    GuardianActionKind::AskUser,
];
const PREFLIGHT_JVM_SECONDARY_VERDICTS: &[GuardianActionKind] =
    &[GuardianActionKind::Fallback, GuardianActionKind::AskUser];
const PREFLIGHT_MANAGED_RUNTIME_MISSING_VERDICTS: &[GuardianActionKind] = &[
    GuardianActionKind::RecordOnly,
    GuardianActionKind::Warn,
    GuardianActionKind::Block,
    GuardianActionKind::Fallback,
    GuardianActionKind::Strip,
    GuardianActionKind::AskUser,
    GuardianActionKind::Repair,
];

const PREFLIGHT_INVARIANT_DIAGNOSIS_RULES: &[PreflightInvariantDiagnosisCopyRule] = &[
    preflight_invariant_diagnosis_rule(
        DiagnosisId::ManagedRuntimeMissing,
        PREFLIGHT_MANAGED_RUNTIME_MISSING_VERDICTS,
        Some("Managed Java runtime is missing and can be prepared before launch."),
        Some("Let Axial prepare the managed Java runtime before launching."),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchMemoryMinClamped,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some(
            "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        ),
        Some(
            "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
        ),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchMemoryAllocationLow,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some("Launch memory allocation is very low for Minecraft."),
        Some(
            "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.",
        ),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchResourceMemoryPressure,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some("Launch memory budget is tight for the current active sessions."),
        Some("Close another running session or lower memory allocation if startup is unstable."),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchResourceCpuPressure,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some(
            "Launch concurrency may be tight: other active launch sessions can saturate low-end CPUs.",
        ),
        Some(
            "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
        ),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchResourceInstallPressure,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some("Active install or download work may add pressure during startup."),
        Some("Wait for active install or download work to finish if startup feels slow."),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::LaunchResourceDiskPressure,
        PREFLIGHT_SUPPORTING_VERDICTS,
        Some("Launch-relevant storage has low free space."),
        Some("Free disk space before launching if caches or natives become unreliable."),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::CustomJavaOverridePresent,
        PREFLIGHT_CUSTOM_VERDICTS,
        Some("Guardian Custom mode will keep the selected Java override for this launch."),
        Some(CUSTOM_MODE_GUIDANCE),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::CustomJvmPresetPresent,
        PREFLIGHT_CUSTOM_VERDICTS,
        Some("Guardian Custom mode will keep the selected JVM preset for this launch."),
        Some(CUSTOM_MODE_GUIDANCE),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::CustomJvmArgsPresent,
        PREFLIGHT_CUSTOM_VERDICTS,
        Some(
            "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.",
        ),
        Some(CUSTOM_MODE_GUIDANCE),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::JvmArgsMalformed,
        PREFLIGHT_JVM_SECONDARY_VERDICTS,
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
        Some(
            "Guardian detected malformed JVM arguments. Fix or remove the explicit JVM args before relying on this launch.",
        ),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::JvmArgUnsupported,
        PREFLIGHT_JVM_SECONDARY_VERDICTS,
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
        Some(
            "Guardian detected JVM flags that may fail on this Java runtime. Remove the explicit JVM args if startup fails.",
        ),
    ),
    preflight_invariant_diagnosis_rule(
        DiagnosisId::JvmArgUnsafeOverride,
        PREFLIGHT_JVM_SECONDARY_VERDICTS,
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
        Some(
            "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly.",
        ),
    ),
];

pub(crate) fn author_guardian_copy(
    request: GuardianCopyRequest<'_>,
) -> Option<GuardianUserOutcome> {
    let GuardianCopyRequest {
        diagnosis_id,
        context,
    } = request;
    match &context {
        GuardianCopyContext::PrepareFailure {
            decision,
            failure_class,
            public_error,
            explicit_java_override_present,
            explicit_jvm_args_present,
            directive,
        } => {
            return Some(author_prepare_failure_copy(
                *decision,
                *failure_class,
                public_error.as_deref(),
                *explicit_java_override_present,
                *explicit_jvm_args_present,
                directive.as_ref(),
            ));
        }
        GuardianCopyContext::StartupFailure {
            decision,
            failure_class,
            stalled,
            first_suspected_mod,
            explicit_java_override_present,
            explicit_jvm_args_present,
            explicit_jvm_preset_present,
            user_mod_set_drift,
            directive,
        } => {
            return Some(author_startup_failure_copy(StartupFailureCopyInput {
                decision: *decision,
                failure_class: *failure_class,
                stalled: *stalled,
                suspected_mod: first_suspected_mod.as_deref(),
                explicit_java: *explicit_java_override_present,
                explicit_jvm_args: *explicit_jvm_args_present,
                explicit_jvm_preset: *explicit_jvm_preset_present,
                user_mod_set_drift: *user_mod_set_drift,
                directive: directive.as_ref(),
            }));
        }
        GuardianCopyContext::ObservedLaunchFailure {
            failure_class,
            observed_phase,
            first_suspected_mod,
        } => {
            return author_observed_launch_failure_copy(
                *failure_class,
                *observed_phase,
                first_suspected_mod.as_deref(),
            );
        }
        GuardianCopyContext::LaunchRecoverySuppressed { directive } => {
            return Some(author_launch_recovery_suppressed_copy(directive));
        }
        _ => {}
    }
    if let GuardianCopyContext::Preflight {
        authored_decision,
        effective_decision,
        phase,
        diagnoses,
        history,
    } = &context
    {
        return author_preflight_copy(
            *authored_decision,
            *effective_decision,
            *phase,
            diagnoses,
            history,
        );
    }
    let decision = context.decision();
    let rule_key = CopyRuleKey {
        diagnosis_id,
        decision,
        context: context.key()?,
    };
    let rule = GUARDIAN_COPY_RULES
        .iter()
        .find(|rule| rule.key == rule_key)?;
    let phase = match rule.phase {
        CopyPhase::Fixed(phase) => phase,
        CopyPhase::PerformanceContext => context.performance_phase()?,
    };
    let summary = trusted_line(rule.summary, MAX_SUMMARY_BYTES);
    let details = finalize_lines(rule.details.iter().map(|line| render_line(*line, &context)));
    let guidance = finalize_lines(
        rule.guidance
            .iter()
            .map(|line| render_line(*line, &context)),
    );

    Some(GuardianUserOutcome::authored(
        decision, phase, summary, details, guidance,
    ))
}

impl GuardianCopyContext<'_> {
    fn decision(&self) -> GuardianActionKind {
        match self {
            Self::RuntimeRepair {
                status: GuardianRepairStatus::Repaired,
            }
            | Self::ArtifactRepair {
                status: GuardianArtifactRepairStatus::Repaired,
            } => GuardianActionKind::Repair,
            Self::RuntimeRepair { .. }
            | Self::ArtifactRepair { .. }
            | Self::PerformanceRejection { .. } => GuardianActionKind::Block,
            Self::InstallFailure { decision, .. } | Self::PersistedStateLoad { decision } => {
                *decision
            }
            Self::Preflight {
                effective_decision, ..
            } => *effective_decision,
            Self::PrepareFailure { decision, .. } | Self::StartupFailure { decision, .. } => {
                *decision
            }
            Self::ObservedLaunchFailure { observed_phase, .. } => match observed_phase {
                GuardianObservedLaunchFailurePhase::BeforeBoot => GuardianActionKind::Block,
                GuardianObservedLaunchFailurePhase::AfterBoot => GuardianActionKind::Warn,
            },
            Self::LaunchRecoverySuppressed { .. } => GuardianActionKind::Block,
        }
    }

    fn key(&self) -> Option<CopyContextKey> {
        match self {
            Self::RuntimeRepair { status } => Some(match status {
                GuardianRepairStatus::Repaired => CopyContextKey::RuntimeRepaired,
                GuardianRepairStatus::Blocked => CopyContextKey::RuntimeBlocked,
                GuardianRepairStatus::Failed => CopyContextKey::RuntimeFailed,
            }),
            Self::ArtifactRepair { status } => Some(match status {
                GuardianArtifactRepairStatus::Repaired => CopyContextKey::ArtifactRepaired,
                GuardianArtifactRepairStatus::Blocked => CopyContextKey::ArtifactBlocked,
                GuardianArtifactRepairStatus::Failed => CopyContextKey::ArtifactFailed,
            }),
            Self::InstallFailure { .. } => Some(CopyContextKey::InstallFailure),
            Self::PerformanceRejection { rejection, .. } => Some(match rejection {
                GuardianPerformanceSupervisionRejection::UnsafeOwnership => {
                    CopyContextKey::PerformanceUnsafeOwnership
                }
                GuardianPerformanceSupervisionRejection::MissingJournal => {
                    CopyContextKey::PerformanceMissingJournal
                }
                GuardianPerformanceSupervisionRejection::UnsafePublicBoundary => {
                    CopyContextKey::PerformanceUnsafePublicBoundary
                }
                GuardianPerformanceSupervisionRejection::GuardianBlocked => {
                    CopyContextKey::PerformanceGuardianBlocked
                }
                GuardianPerformanceSupervisionRejection::FallbackUnavailable => {
                    CopyContextKey::PerformanceFallbackUnavailable
                }
                GuardianPerformanceSupervisionRejection::RollbackUnavailable => {
                    CopyContextKey::PerformanceRollbackUnavailable
                }
            }),
            Self::PersistedStateLoad { .. } => Some(CopyContextKey::PersistedStateLoad),
            Self::Preflight { .. }
            | Self::PrepareFailure { .. }
            | Self::StartupFailure { .. }
            | Self::ObservedLaunchFailure { .. }
            | Self::LaunchRecoverySuppressed { .. } => None,
        }
    }

    fn performance_phase(&self) -> Option<OperationPhase> {
        match self {
            Self::PerformanceRejection { phase, .. } => Some(*phase),
            _ => None,
        }
    }
}

fn render_line(line: CopyLine, context: &GuardianCopyContext<'_>) -> String {
    match line {
        CopyLine::Static(value) => trusted_line(value, MAX_LINE_BYTES),
        CopyLine::RuntimeUnavailableDetail => {
            let (component, platform) = match install_dynamics(context) {
                InstallCopyDynamics::RuntimeUnavailable {
                    component,
                    platform,
                } => (
                    sanitize_dynamic_token(component),
                    sanitize_dynamic_token(platform),
                ),
                InstallCopyDynamics::None | InstallCopyDynamics::Rosetta { .. } => (None, None),
            };
            let component = component.unwrap_or_else(|| "the required runtime".to_string());
            let platform = platform.unwrap_or_else(|| "this device".to_string());
            checked_rendered_line(format!(
                "Java runtime component {component} is not available for {platform}."
            ))
        }
        CopyLine::RuntimeRosettaDetail => {
            let component = match install_dynamics(context) {
                InstallCopyDynamics::Rosetta { component } => sanitize_dynamic_token(component),
                InstallCopyDynamics::None | InstallCopyDynamics::RuntimeUnavailable { .. } => None,
            }
            .unwrap_or_else(|| "the required runtime".to_string());
            checked_rendered_line(format!(
                "Java runtime component {component} needs Rosetta 2 on this Mac."
            ))
        }
    }
}

struct AcceptedLaunchFailureCopy {
    startup_detail: String,
    running_summary: String,
    running_detail: String,
}

fn first_suspected_mod(crash_evidence: Option<&CrashEvidence>) -> Option<String> {
    crash_evidence
        .and_then(|evidence| evidence.suspected_mods.first())
        .and_then(|suspected_mod| {
            sanitize_evidence_text(
                suspected_mod.name.as_str(),
                RedactionAudience::UserVisible,
                MAX_DYNAMIC_TOKEN_BYTES,
            )
            .filter(|suspected_mod| suspected_mod.len() <= MAX_DYNAMIC_TOKEN_BYTES)
        })
}

fn accepted_failure_copy(
    failure_class: LaunchFailureClass,
    suspected_mod: Option<&str>,
) -> Option<AcceptedLaunchFailureCopy> {
    let copy = match failure_class {
        LaunchFailureClass::OutOfMemory => (
            "Minecraft exited before startup completed after running out of memory.",
            "Minecraft stopped after running out of memory.",
            "Guardian detected an out-of-memory crash after startup completed.",
        ),
        LaunchFailureClass::GraphicsDriverCrash => (
            "Minecraft exited before startup completed with a detected graphics driver crash.",
            "Minecraft stopped after a graphics driver crash.",
            "Guardian detected a native graphics driver crash after startup completed.",
        ),
        LaunchFailureClass::MissingDependency => (
            "Minecraft exited before startup completed because a required dependency was missing.",
            "Minecraft stopped because a dependency was missing.",
            "Guardian detected a missing class or dependency after startup completed.",
        ),
        LaunchFailureClass::ModTransformationFailure => (
            "Minecraft exited before startup completed with a detected mod transformation or mixin failure.",
            "Minecraft stopped during mod transformation.",
            "Guardian detected a mod transformation or mixin failure after startup completed.",
        ),
        LaunchFailureClass::ModAttributedCrash => {
            return Some(AcceptedLaunchFailureCopy {
                startup_detail: suspected_mod
                    .map(|name| format!("Minecraft exited before startup completed with a crash attributed to {name}."))
                    .unwrap_or_else(|| "Minecraft exited before startup completed with a crash attributed to an installed mod.".to_string()),
                running_summary: suspected_mod
                    .map(|name| format!("Minecraft stopped in a crash attributed to {name}."))
                    .unwrap_or_else(|| "Minecraft stopped in a mod-attributed crash.".to_string()),
                running_detail: suspected_mod
                    .map(|name| format!("Guardian attributes the crash to the installed mod {name}."))
                    .unwrap_or_else(|| "Guardian found typed crash evidence that attributes the failure to an installed mod.".to_string()),
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
    })
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
        LaunchFailureClass::Unknown
        | LaunchFailureClass::ClasspathModuleConflict
        | LaunchFailureClass::LauncherManagedArtifactSignature
        | LaunchFailureClass::AuthModeIncompatible
        | LaunchFailureClass::LoaderBootstrapFailure
        | LaunchFailureClass::StartupStalled
        | LaunchFailureClass::OutOfMemory
        | LaunchFailureClass::GraphicsDriverCrash
        | LaunchFailureClass::MissingDependency
        | LaunchFailureClass::ModTransformationFailure
        | LaunchFailureClass::ModAttributedCrash => {
            "Launch preparation failed before Minecraft could start."
        }
    }
}

#[derive(Clone, Copy)]
struct LaunchFailureGuidanceContext<'a> {
    phase: OperationPhase,
    explicit_java: bool,
    explicit_jvm_args: bool,
    explicit_jvm_preset: bool,
    suspected_mod: Option<&'a str>,
}

fn launch_failure_guidance(
    failure_class: LaunchFailureClass,
    context: LaunchFailureGuidanceContext<'_>,
) -> String {
    match failure_class {
        LaunchFailureClass::Unknown
            if context.explicit_java
                || context.explicit_jvm_args
                || context.explicit_jvm_preset =>
        {
            trusted_line(
                "Review recent Java, JVM preset, or JVM argument overrides before retrying.",
                MAX_LINE_BYTES,
            )
        }
        LaunchFailureClass::Unknown if context.phase == OperationPhase::Preparing => trusted_line(
            "Review the reported failure and recent launch-setting changes before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::Unknown => trusted_line(
            "Review the latest game log before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering
            if context.explicit_jvm_args =>
        {
            trusted_line(
                "Remove the explicit JVM args or switch Guardian Mode back to Managed.",
                MAX_LINE_BYTES,
            )
        }
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering
            if context.explicit_jvm_preset =>
        {
            trusted_line(
                "Choose a safer JVM preset or switch Guardian Mode back to Managed.",
                MAX_LINE_BYTES,
            )
        }
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => trusted_line(
            "Use safer launch settings or let Axial manage compatibility.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::JavaRuntimeMismatch if context.explicit_java => trusted_line(
            "Remove the Java override or switch Guardian Mode back to Managed.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::JavaRuntimeMismatch => trusted_line(
            "Use a compatible Java runtime or let Axial use the managed runtime.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::OutOfMemory => trusted_line(
            "Review the instance memory allocation and close memory-heavy apps before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::GraphicsDriverCrash => trusted_line(
            "Update or reinstall the graphics driver, then retry without graphics overlays.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::MissingDependency => trusted_line(
            "Check the installed mods for missing or incompatible dependencies before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::ModTransformationFailure => trusted_line(
            "Update or remove the recently changed mod before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::ModAttributedCrash => context
            .suspected_mod
            .map(|name| checked_rendered_line(format!("Update or remove {name} before retrying.")))
            .unwrap_or_else(|| {
                trusted_line(
                    "Update or remove the suspected mod before retrying.",
                    MAX_LINE_BYTES,
                )
            }),
        LaunchFailureClass::ClasspathModuleConflict => trusted_line(
            "Check for incompatible mods or libraries that alter the classpath before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::LauncherManagedArtifactSignature => trusted_line(
            "Repair the installed version so Axial can replace the affected launcher-managed jars.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::AuthModeIncompatible => trusted_line(
            "Verify the selected account and authentication mode before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::LoaderBootstrapFailure => trusted_line(
            "Repair or reinstall the selected mod loader before retrying.",
            MAX_LINE_BYTES,
        ),
        LaunchFailureClass::StartupStalled
            if context.explicit_java
                || context.explicit_jvm_args
                || context.explicit_jvm_preset =>
        {
            trusted_line(
                "Review recent Java, JVM preset, or JVM argument overrides before retrying.",
                MAX_LINE_BYTES,
            )
        }
        LaunchFailureClass::StartupStalled if context.phase == OperationPhase::Preparing => {
            trusted_line(
                "Launch stalled before startup. Review recent override changes first.",
                MAX_LINE_BYTES,
            )
        }
        LaunchFailureClass::StartupStalled => trusted_line(
            "Review the latest game log before retrying.",
            MAX_LINE_BYTES,
        ),
    }
}

fn author_prepare_failure_copy(
    decision: GuardianActionKind,
    failure_class: LaunchFailureClass,
    public_error: Option<&str>,
    explicit_java: bool,
    explicit_jvm_args: bool,
    directive: Option<&GuardianDirective>,
) -> GuardianUserOutcome {
    let detail = directive
        .map(guardian_directive_description)
        .or_else(|| public_error.map(ToOwned::to_owned))
        .unwrap_or_else(|| prepare_failure_reason(failure_class).to_string());
    let summary = match decision {
        GuardianActionKind::Fallback | GuardianActionKind::Strip => {
            "Guardian adjusted launch preparation."
        }
        GuardianActionKind::AskUser => "Guardian needs confirmation before launch preparation.",
        GuardianActionKind::Block => "Guardian blocked launch preparation.",
        _ => "Guardian recorded launch preparation failure.",
    };
    GuardianUserOutcome::authored(
        launch_public_decision(decision),
        OperationPhase::Preparing,
        trusted_line(summary, MAX_SUMMARY_BYTES),
        finalize_launch_lines([detail]),
        finalize_launch_lines([launch_failure_guidance(
            failure_class,
            LaunchFailureGuidanceContext {
                phase: OperationPhase::Preparing,
                explicit_java,
                explicit_jvm_args,
                explicit_jvm_preset: false,
                suspected_mod: None,
            },
        )]),
    )
}

fn startup_failure_reason(
    failure_class: LaunchFailureClass,
    stalled: bool,
    suspected_mod: Option<&str>,
) -> String {
    if let Some(copy) = accepted_failure_copy(failure_class, suspected_mod) {
        return copy.startup_detail;
    }
    if stalled {
        return "No startup activity was observed before the startup window ended.".to_string();
    }
    match failure_class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => "Minecraft exited before startup completed with a detected JVM option compatibility failure.",
        LaunchFailureClass::JavaRuntimeMismatch => "Minecraft exited before startup completed with a detected Java runtime mismatch.",
        LaunchFailureClass::ClasspathModuleConflict => "Minecraft exited before startup completed with a detected classpath or module conflict.",
        LaunchFailureClass::LauncherManagedArtifactSignature => "Minecraft exited before startup completed with detected launcher-managed jar signature corruption.",
        LaunchFailureClass::AuthModeIncompatible => "Minecraft exited before startup completed because the selected auth mode was not launch-ready.",
        LaunchFailureClass::LoaderBootstrapFailure => "Minecraft exited before startup completed with a detected loader bootstrap failure.",
        LaunchFailureClass::StartupStalled => "Minecraft exited before startup completed after startup activity stalled.",
        LaunchFailureClass::Unknown
        | LaunchFailureClass::OutOfMemory
        | LaunchFailureClass::GraphicsDriverCrash
        | LaunchFailureClass::MissingDependency
        | LaunchFailureClass::ModTransformationFailure
        | LaunchFailureClass::ModAttributedCrash => {
            "Minecraft exited before Guardian could verify a completed startup."
        }
    }.to_string()
}

struct StartupFailureCopyInput<'a> {
    decision: GuardianActionKind,
    failure_class: LaunchFailureClass,
    stalled: bool,
    suspected_mod: Option<&'a str>,
    explicit_java: bool,
    explicit_jvm_args: bool,
    explicit_jvm_preset: bool,
    user_mod_set_drift: bool,
    directive: Option<&'a GuardianDirective>,
}

fn author_startup_failure_copy(input: StartupFailureCopyInput<'_>) -> GuardianUserOutcome {
    let StartupFailureCopyInput {
        decision,
        failure_class,
        stalled,
        suspected_mod,
        explicit_java,
        explicit_jvm_args,
        explicit_jvm_preset,
        user_mod_set_drift,
        directive,
    } = input;
    let guidance = launch_failure_guidance(
        failure_class,
        LaunchFailureGuidanceContext {
            phase: OperationPhase::Launching,
            explicit_java,
            explicit_jvm_args,
            explicit_jvm_preset,
            suspected_mod,
        },
    );
    let summary = match decision {
        GuardianActionKind::Downgrade
        | GuardianActionKind::Strip
        | GuardianActionKind::Fallback => "Guardian selected a guarded startup retry.",
        GuardianActionKind::AskUser => "Guardian needs confirmation before startup recovery.",
        GuardianActionKind::Block => "Guardian blocked launch startup.",
        _ => "Guardian recorded launch startup failure.",
    };
    let mut details = vec![
        directive
            .map(guardian_directive_description)
            .unwrap_or_else(|| startup_failure_reason(failure_class, stalled, suspected_mod)),
    ];
    if user_mod_set_drift {
        details.push("The active mods changed since the last successful launch.".to_string());
    }
    GuardianUserOutcome::authored(
        launch_public_decision(decision),
        OperationPhase::Launching,
        trusted_line(summary, MAX_SUMMARY_BYTES),
        finalize_launch_lines(details),
        finalize_launch_lines([guidance]),
    )
}

fn author_observed_launch_failure_copy(
    failure_class: LaunchFailureClass,
    observed_phase: GuardianObservedLaunchFailurePhase,
    suspected_mod: Option<&str>,
) -> Option<GuardianUserOutcome> {
    let copy = accepted_failure_copy(failure_class, suspected_mod)?;
    let (decision, phase, summary, detail) = match observed_phase {
        GuardianObservedLaunchFailurePhase::BeforeBoot => (
            GuardianActionKind::Block,
            OperationPhase::Launching,
            "Guardian blocked launch startup.".to_string(),
            copy.startup_detail,
        ),
        GuardianObservedLaunchFailurePhase::AfterBoot => (
            GuardianActionKind::Warn,
            OperationPhase::Running,
            copy.running_summary,
            copy.running_detail,
        ),
    };
    Some(GuardianUserOutcome::authored(
        decision,
        phase,
        launch_summary(&summary),
        finalize_launch_lines([detail]),
        finalize_launch_lines([launch_failure_guidance(
            failure_class,
            LaunchFailureGuidanceContext {
                phase,
                explicit_java: false,
                explicit_jvm_args: false,
                explicit_jvm_preset: false,
                suspected_mod,
            },
        )]),
    ))
}

fn author_launch_recovery_suppressed_copy(directive: &GuardianDirective) -> GuardianUserOutcome {
    let label = guardian_directive_recovery_label(directive);
    let detail = checked_rendered_line(format!(
        "Guardian suppressed a repeated launch self-healing retry for {label} because the same recovery failed recently."
    ));
    GuardianUserOutcome::authored(
        GuardianActionKind::Block,
        OperationPhase::Repairing,
        detail.clone(),
        vec![detail],
        vec![
            "Review the latest game log or change the affected launch setting before retrying."
                .to_string(),
        ],
    )
}

fn launch_public_decision(decision: GuardianActionKind) -> GuardianActionKind {
    match decision {
        GuardianActionKind::Fallback
        | GuardianActionKind::Strip
        | GuardianActionKind::Downgrade
        | GuardianActionKind::Retry
        | GuardianActionKind::AskUser
        | GuardianActionKind::Block
        | GuardianActionKind::Allow
        | GuardianActionKind::RecordOnly => decision,
        _ => GuardianActionKind::Warn,
    }
}

fn author_preflight_copy(
    authored_decision: GuardianActionKind,
    effective_decision: GuardianActionKind,
    phase: OperationPhase,
    diagnoses: &[DiagnosisId],
    history: &[PreflightHistory],
) -> Option<GuardianUserOutcome> {
    if authored_decision != effective_decision
        && !(authored_decision == GuardianActionKind::AskUser
            && effective_decision == GuardianActionKind::Block)
    {
        return None;
    }
    let summary = PREFLIGHT_SUMMARY_RULES
        .iter()
        .find(|rule| rule.decision == authored_decision)
        .map(|rule| trusted_line(rule.summary, MAX_SUMMARY_BYTES))?;
    let diagnosis_lines = diagnoses
        .iter()
        .filter_map(|diagnosis_id| preflight_diagnosis_copy(*diagnosis_id, authored_decision))
        .collect::<Vec<_>>();
    let history_lines = history
        .iter()
        .map(render_preflight_history)
        .collect::<Vec<_>>();
    let ordered = if authored_decision == GuardianActionKind::Warn {
        history_lines.iter().chain(&diagnosis_lines)
    } else {
        diagnosis_lines.iter().chain(&history_lines)
    };
    let details = finalize_lines(ordered.clone().filter_map(|lines| lines.0.clone()));
    let guidance = finalize_lines(ordered.filter_map(|lines| lines.1.clone()));

    Some(GuardianUserOutcome::authored(
        effective_decision,
        phase,
        summary,
        details,
        guidance,
    ))
}

fn preflight_diagnosis_copy(
    diagnosis_id: DiagnosisId,
    decision: GuardianActionKind,
) -> Option<(Option<String>, Option<String>)> {
    let coordinate = PreflightCopyCoordinate {
        diagnosis_id,
        decision,
    };
    if let Some(rule) = PREFLIGHT_DIAGNOSIS_RULES
        .iter()
        .find(|rule| rule.coordinate == coordinate)
    {
        return Some((
            rule.detail.map(|line| trusted_line(line, MAX_LINE_BYTES)),
            rule.guidance.map(|line| trusted_line(line, MAX_LINE_BYTES)),
        ));
    }
    let rule = PREFLIGHT_INVARIANT_DIAGNOSIS_RULES
        .iter()
        .find(|rule| rule.diagnosis_id == diagnosis_id && rule.decisions.contains(&decision))?;
    Some((
        rule.detail.map(|line| trusted_line(line, MAX_LINE_BYTES)),
        rule.guidance.map(|line| trusted_line(line, MAX_LINE_BYTES)),
    ))
}

fn preflight_history(facts: &[GuardianFact]) -> Vec<PreflightHistory> {
    facts.iter().filter_map(preflight_history_fact).collect()
}

fn preflight_history_fact(fact: &GuardianFact) -> Option<PreflightHistory> {
    match fact.id {
        super::GuardianFactId::RecentStartupFailure => preflight_startup_history(fact),
        super::GuardianFactId::RecentRepairFailed => {
            let recovery = match copy_fact_field(fact, "diagnosis")? {
                value if value == DiagnosisId::JavaRuntimeRecovery.as_str() => {
                    PreflightRecoveryKind::JavaRuntime
                }
                value if value == DiagnosisId::JvmArgUnsupported.as_str() => {
                    PreflightRecoveryKind::JvmArgs
                }
                value if value == DiagnosisId::JvmPresetRecovery.as_str() => {
                    PreflightRecoveryKind::JvmPreset
                }
                _ => return None,
            };
            Some(PreflightHistory::RepairFailed(recovery))
        }
        super::GuardianFactId::RepairSuppressedUntil => {
            let timestamp =
                DateTime::parse_from_rfc3339(copy_fact_field(fact, "suppression_until")?)
                    .ok()?
                    .with_timezone(&Utc);
            Some(PreflightHistory::Suppressed(PreflightSuppressionTime {
                hour: timestamp.hour(),
                minute: timestamp.minute(),
            }))
        }
        _ => None,
    }
}

fn preflight_startup_history(fact: &GuardianFact) -> Option<PreflightHistory> {
    let class =
        match copy_fact_field(fact, "failure_class").and_then(LaunchFailureClass::from_name)? {
            LaunchFailureClass::OutOfMemory => PreflightCrashClass::OutOfMemory,
            LaunchFailureClass::GraphicsDriverCrash => PreflightCrashClass::GraphicsDriverCrash,
            LaunchFailureClass::MissingDependency => PreflightCrashClass::MissingDependency,
            LaunchFailureClass::ModTransformationFailure => {
                PreflightCrashClass::ModTransformationFailure
            }
            LaunchFailureClass::ModAttributedCrash => PreflightCrashClass::ModAttributedCrash,
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
    let occurrences = copy_fact_field_u32(fact, "occurrences").filter(|count| *count > 0);
    let latest_today = copy_fact_field(fact, "latest_observed_today") == Some("true");
    let occurrences_today = latest_today
        .then(|| copy_fact_field_u32(fact, "occurrences_today"))
        .flatten()
        .filter(|count| *count > 0)
        .filter(|count| occurrences.is_none_or(|total| *count <= total));
    let window = if let Some(count) = occurrences_today {
        PreflightOccurrenceWindow::Today(count)
    } else if let Some(count) = occurrences {
        PreflightOccurrenceWindow::Total {
            count,
            latest_today,
        }
    } else {
        PreflightOccurrenceWindow::Recent
    };
    let oom_budget = (class == PreflightCrashClass::OutOfMemory).then(|| {
        let current = copy_fact_field_u32(fact, "current_memory_mb").filter(|value| *value > 0);
        let suggested = copy_fact_field_u32(fact, "suggested_memory_mb").filter(|value| *value > 0);
        match (current, suggested) {
            (Some(current_mb), Some(suggested_mb)) if suggested_mb > current_mb => {
                PreflightOomBudget::Concrete {
                    current_mb,
                    suggested_mb,
                }
            }
            _ => PreflightOomBudget::Unverified,
        }
    });
    Some(PreflightHistory::StartupFailure {
        class,
        window,
        oom_budget,
    })
}

fn render_preflight_history(history: &PreflightHistory) -> (Option<String>, Option<String>) {
    match history {
        PreflightHistory::StartupFailure {
            class,
            window,
            oom_budget,
        } => (
            Some(checked_rendered_line(render_startup_failure_detail(
                *class, *window,
            ))),
            startup_failure_guidance(*class, *oom_budget).map(checked_rendered_line),
        ),
        PreflightHistory::RepairFailed(recovery) => {
            let (detail, guidance) = match recovery {
                PreflightRecoveryKind::JavaRuntime => (
                    "The previous managed Java recovery attempt failed.",
                    "Review the selected Java runtime before relaunching.",
                ),
                PreflightRecoveryKind::JvmArgs => (
                    "The previous JVM argument recovery attempt failed.",
                    "Review or remove explicit JVM arguments before relaunching.",
                ),
                PreflightRecoveryKind::JvmPreset => (
                    "The previous JVM preset recovery attempt failed.",
                    "Review the JVM preset before relaunching.",
                ),
            };
            (
                Some(trusted_line(detail, MAX_LINE_BYTES)),
                Some(trusted_line(guidance, MAX_LINE_BYTES)),
            )
        }
        PreflightHistory::Suppressed(time) => (
            Some(checked_rendered_line(format!(
                "Guardian will not auto-repair this launch again until {:02}:{:02} UTC.",
                time.hour, time.minute
            ))),
            Some(checked_rendered_line(format!(
                "Review the launch settings before retrying; unchanged settings will not trigger another automatic repair before {:02}:{:02} UTC.",
                time.hour, time.minute
            ))),
        ),
    }
}

#[derive(Clone, Copy)]
struct PreflightCrashLabel {
    singular: &'static str,
    plural: &'static str,
    with_article: &'static str,
}

fn render_startup_failure_detail(
    class: PreflightCrashClass,
    window: PreflightOccurrenceWindow,
) -> String {
    let label = preflight_crash_label(class);
    match window {
        PreflightOccurrenceWindow::Today(count) => {
            counted_preflight_failure("had", count, label, " today")
        }
        PreflightOccurrenceWindow::Total {
            count,
            latest_today,
        } => counted_preflight_failure(
            "has recorded",
            count,
            label,
            if latest_today {
                "; the latest was today"
            } else {
                "; the latest was within the past 24 hours"
            },
        ),
        PreflightOccurrenceWindow::Recent => {
            format!("A recent launch ended with {}.", label.with_article)
        }
    }
}

fn counted_preflight_failure(
    verb: &str,
    count: u32,
    label: PreflightCrashLabel,
    suffix: &str,
) -> String {
    if count == 1 {
        format!("This instance {verb} one {}{suffix}.", label.singular)
    } else {
        format!("This instance {verb} {count} {}{suffix}.", label.plural)
    }
}

fn preflight_crash_label(class: PreflightCrashClass) -> PreflightCrashLabel {
    match class {
        PreflightCrashClass::OutOfMemory => PreflightCrashLabel {
            singular: "out-of-memory crash",
            plural: "out-of-memory crashes",
            with_article: "an out-of-memory crash",
        },
        PreflightCrashClass::GraphicsDriverCrash => PreflightCrashLabel {
            singular: "graphics driver crash",
            plural: "graphics driver crashes",
            with_article: "a graphics driver crash",
        },
        PreflightCrashClass::MissingDependency => PreflightCrashLabel {
            singular: "missing-dependency crash",
            plural: "missing-dependency crashes",
            with_article: "a missing-dependency crash",
        },
        PreflightCrashClass::ModTransformationFailure => PreflightCrashLabel {
            singular: "mod transformation crash",
            plural: "mod transformation crashes",
            with_article: "a mod transformation crash",
        },
        PreflightCrashClass::ModAttributedCrash => PreflightCrashLabel {
            singular: "mod-attributed crash",
            plural: "mod-attributed crashes",
            with_article: "a mod-attributed crash",
        },
    }
}

fn startup_failure_guidance(
    class: PreflightCrashClass,
    oom_budget: Option<PreflightOomBudget>,
) -> Option<String> {
    Some(match class {
        PreflightCrashClass::OutOfMemory => match oom_budget? {
            PreflightOomBudget::Concrete {
                current_mb,
                suggested_mb,
            } => format!(
                "Increase this instance's maximum memory from {current_mb} MB to {suggested_mb} MB before relaunching."
            ),
            PreflightOomBudget::Unverified =>
                "Guardian could not verify safe headroom for a larger memory allocation. Close another session or free memory before relaunching."
                    .to_string(),
        },
        PreflightCrashClass::GraphicsDriverCrash =>
            "Update the graphics driver and remove graphics overrides before relaunching."
                .to_string(),
        PreflightCrashClass::MissingDependency =>
            "Repair the instance dependencies before relaunching.".to_string(),
        PreflightCrashClass::ModTransformationFailure =>
            "Review recently changed mods and their loader compatibility before relaunching."
                .to_string(),
        PreflightCrashClass::ModAttributedCrash =>
            "Review recently changed mods and disable the suspected mod before relaunching."
                .to_string(),
    })
}

fn copy_fact_field<'a>(fact: &'a GuardianFact, key: &str) -> Option<&'a str> {
    let mut values = fact
        .fields
        .iter()
        .filter(|field| field.key == key)
        .filter_map(|field| field.value_for(RedactionAudience::UserVisible));
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn copy_fact_field_u32(fact: &GuardianFact, key: &str) -> Option<u32> {
    copy_fact_field(fact, key)?.parse().ok()
}

fn install_dynamics<'a>(context: &'a GuardianCopyContext<'a>) -> InstallCopyDynamics<'a> {
    match context {
        GuardianCopyContext::InstallFailure { dynamics, .. } => *dynamics,
        _ => InstallCopyDynamics::None,
    }
}

fn install_copy_dynamics<'a>(
    diagnosis_id: DiagnosisId,
    evidence: &'a [GuardianInstallArtifactFailureEvidence],
) -> InstallCopyDynamics<'a> {
    let kind = match diagnosis_id {
        DiagnosisId::ManagedRuntimeUnavailableForPlatform => {
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform
        }
        DiagnosisId::ManagedRuntimeRosettaRequired => {
            GuardianInstallArtifactFailureKind::RuntimeRosettaRequired
        }
        _ => return InstallCopyDynamics::None,
    };
    let Some(evidence) = evidence.iter().find(|evidence| evidence.kind == kind) else {
        return InstallCopyDynamics::None;
    };
    let field = |key| {
        evidence
            .fields
            .iter()
            .find(|(field_key, _)| field_key == key)
            .map(|(_, value)| value.as_str())
    };
    match kind {
        GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform => {
            InstallCopyDynamics::RuntimeUnavailable {
                component: field("component"),
                platform: field("platform"),
            }
        }
        GuardianInstallArtifactFailureKind::RuntimeRosettaRequired => {
            InstallCopyDynamics::Rosetta {
                component: field("component"),
            }
        }
        _ => InstallCopyDynamics::None,
    }
}

fn sanitize_dynamic_token(value: Option<&str>) -> Option<String> {
    sanitize_evidence_token(
        value?,
        RedactionAudience::UserVisible,
        MAX_DYNAMIC_TOKEN_BYTES,
    )
    .filter(|value| value.len() <= MAX_DYNAMIC_TOKEN_BYTES)
}

fn trusted_line(value: &'static str, max_bytes: usize) -> String {
    assert!(!value.is_empty() && value.len() <= max_bytes);
    value.to_string()
}

fn checked_rendered_line(value: String) -> String {
    assert!(!value.is_empty() && value.len() <= MAX_LINE_BYTES);
    value
}

fn launch_summary(value: &str) -> String {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, MAX_SUMMARY_BYTES)
        .filter(|value| value.len() <= MAX_SUMMARY_BYTES)
        .unwrap_or_else(|| "Guardian recorded launch safety outcome.".to_string())
}

fn finalize_lines(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = Vec::new();
    for line in lines {
        assert!(!line.is_empty() && line.len() <= MAX_LINE_BYTES);
        if values.iter().any(|existing| existing == &line) {
            continue;
        }
        values.push(line);
        if values.len() == MAX_COLLECTION_LINES {
            break;
        }
    }
    values
}

fn finalize_launch_lines(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = Vec::new();
    for line in lines {
        let Some(line) =
            sanitize_evidence_text(&line, RedactionAudience::UserVisible, MAX_LINE_BYTES)
                .filter(|line| line.len() <= MAX_LINE_BYTES)
        else {
            continue;
        };
        if values.iter().any(|existing| existing == &line) {
            continue;
        }
        values.push(line);
        if values.len() == MAX_COLLECTION_LINES {
            break;
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::{
        CopyContextKey, GUARDIAN_COPY_RULES, GUARDIAN_JVM_PRESET_COPY_RULES, GuardianCopyRequest,
        GuardianInterventionKind, GuardianRuntimeRepairCopy, GuardianSummary,
        GuardianSummaryDecision, MAX_COLLECTION_LINES, MAX_LINE_BYTES, MAX_SUMMARY_BYTES,
        PREFLIGHT_DIAGNOSIS_RULES, PREFLIGHT_INVARIANT_DIAGNOSIS_RULES, PREFLIGHT_SUMMARY_RULES,
        author_guardian_copy, finalize_lines, guardian_install_outcome_persistence_facts,
        guardian_summary_for_test, guardian_summary_from_persisted_value,
    };
    use crate::guardian::{
        DiagnosisId, GuardianActionKind, GuardianArtifactRepairStatus,
        GuardianInstallArtifactFailureEvidence, GuardianInstallArtifactFailureKind,
        GuardianJvmPresetId, GuardianLaunchAdmission, GuardianManagedJavaReason,
        GuardianPerformanceSupervisionRejection, GuardianRepairStatus, GuardianStripJvmArgsReason,
        GuardianUserOutcome, SafetyOutcome,
    };
    use crate::state::contracts::OperationPhase;
    use axial_launcher::{
        GuardianMode, LaunchHealingSummary, LaunchNoticeTone, LaunchSessionExitReason,
        LaunchSessionOutcome, LaunchSessionRecord, LaunchState, SessionId,
    };

    fn warning_summary() -> GuardianSummary {
        GuardianSummary {
            mode: GuardianMode::Managed,
            decision: GuardianSummaryDecision::Warned,
            message: Some("Guardian flagged launch settings for review.".to_string()),
            details: vec!["Keep existing launch guidance.".to_string()],
            guidance: vec!["Keep existing launch guidance.".to_string()],
            interventions: Vec::new(),
        }
    }

    fn empty_summary(mode: GuardianMode) -> GuardianSummary {
        guardian_summary_for_test(
            mode,
            GuardianSummaryDecision::Allowed,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn persisted_guardian_summary_round_trips_the_current_wire_shape() {
        let expected = warning_summary();
        let value = serde_json::to_value(&expected).expect("serialize Guardian summary");
        let parsed = guardian_summary_from_persisted_value(&value).expect("current Guardian shape");

        assert_eq!(
            serde_json::to_value(parsed).expect("serialize parsed summary"),
            value
        );
    }

    #[test]
    fn persisted_guardian_summary_is_strict_redacted_and_bounded() {
        let user_visible_only = "x".repeat(200);
        let mut value = serde_json::json!({
            "mode": "managed",
            "decision": "warned",
            "message": "/home/alice/.axial token=secret",
            "details": std::iter::once(user_visible_only.clone())
                .chain((0..8).map(|index| format!("detail {index}")))
                .collect::<Vec<_>>(),
            "guidance": ["C:\\Users\\Alice\\secret", "Safe guidance"],
            "interventions": [{
                "kind": "switch_managed_runtime",
                "detail": "--accessToken secret",
                "public_detail": "Managed Java selected.",
                "silent": false
            }]
        });
        let parsed = guardian_summary_from_persisted_value(&value).expect("bounded Guardian shape");
        let encoded = serde_json::to_string(&parsed).expect("serialize bounded Guardian summary");

        assert_eq!(parsed.details.len(), MAX_COLLECTION_LINES);
        assert_eq!(parsed.details.first(), Some(&user_visible_only));
        assert!(parsed.guidance.len() <= MAX_COLLECTION_LINES);
        assert!(
            parsed
                .details
                .iter()
                .all(|line| line.len() <= MAX_LINE_BYTES)
        );
        assert!(!encoded.to_ascii_lowercase().contains("secret"));
        assert!(!encoded.contains("/home/alice"));
        assert!(!encoded.contains("C:\\\\Users"));

        value["unknown"] = serde_json::json!(true);
        let parsed = guardian_summary_from_persisted_value(&value).expect("known Guardian fields");
        let encoded = serde_json::to_value(parsed).expect("serialize known Guardian fields");
        assert!(encoded.get("unknown").is_none());
    }

    #[test]
    fn launch_notice_preserves_guardian_precedence_and_healing_fallback() {
        let guardian = GuardianSummary {
            mode: GuardianMode::Managed,
            decision: GuardianSummaryDecision::Blocked,
            message: Some("Guardian blocked an unsafe launch setup.".to_string()),
            details: vec!["Custom Java path is unavailable.".to_string()],
            guidance: Vec::new(),
            interventions: Vec::new(),
        };
        let healing = LaunchHealingSummary {
            failure_class: Some("java_runtime_mismatch".to_string()),
            ..Default::default()
        };

        let notice = super::launch_notice(
            Some(&guardian),
            Some(&healing),
            None,
            Some("Fallback detail"),
            Some("Fallback message"),
        )
        .expect("Guardian notice");
        assert_eq!(notice.message, "Guardian blocked an unsafe launch setup.");
        assert_eq!(notice.details, ["Custom Java path is unavailable."]);
        assert_eq!(notice.tone, LaunchNoticeTone::Error);

        let healing_notice = super::launch_notice(
            None,
            Some(&healing),
            None,
            Some("Java check failed"),
            Some("Fallback message"),
        )
        .expect("Healing notice");
        assert_eq!(
            healing_notice.message,
            "Launch stopped before startup because the required Java runtime was not available."
        );
        assert_eq!(healing_notice.detail.as_deref(), Some("Java check failed."));
        assert_eq!(healing_notice.tone, LaunchNoticeTone::Error);
    }

    #[test]
    fn launch_notice_rejects_malformed_values_and_uses_only_visible_intervention_copy() {
        let malformed = serde_json::json!({
            "mode": "managed",
            "decision": "not_a_decision"
        });
        assert!(
            super::launch_notice_from_values(Some(&malformed), None, None, None, None).is_none()
        );

        let directive = crate::guardian::GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::PrepareFailure,
        };
        let visible = super::guardian_summary_with_intervention(
            &empty_summary(GuardianMode::Managed),
            &directive,
            false,
        );
        let visible_notice = super::launch_notice(Some(&visible), None, None, None, None)
            .expect("visible intervention notice");
        assert_eq!(
            visible_notice.details,
            ["Guardian used the managed Java runtime for this launch."]
        );
        assert!(
            !visible_notice
                .details
                .iter()
                .any(|detail| detail.contains("switched to managed Java before launch"))
        );

        let silent = super::guardian_summary_with_intervention(
            &empty_summary(GuardianMode::Managed),
            &directive,
            true,
        );
        let silent_notice = super::launch_notice(Some(&silent), None, None, None, None)
            .expect("silent intervention summary still has a message");
        assert!(silent_notice.details.is_empty());
    }

    #[test]
    fn launch_notice_enforces_privacy_and_bounds_at_the_authority() {
        let guardian = GuardianSummary {
            mode: GuardianMode::Managed,
            decision: GuardianSummaryDecision::Warned,
            message: Some(
                "Leaked /home/alice/.minecraft --accessToken raw-secret-token".to_string(),
            ),
            details: ["/Users/Alice/private/java -Xmx8192M raw-secret-token".to_string()]
                .into_iter()
                .chain((0..10).map(|index| format!("Safe bounded detail {index}.")))
                .collect(),
            guidance: Vec::new(),
            interventions: Vec::new(),
        };

        let notice = super::launch_notice(Some(&guardian), None, None, None, None)
            .expect("bounded Guardian notice");
        assert_eq!(notice.message, super::PRIVATE_NOTICE_FALLBACK);
        assert_eq!(notice.details.len(), super::MAX_NOTICE_DETAILS);
        assert!(
            notice
                .details
                .iter()
                .all(|detail| detail.len() <= super::MAX_NOTICE_DETAIL_BYTES)
        );
        let encoded = serde_json::to_string(&notice).expect("notice JSON");
        assert!(!encoded.contains("alice"));
        assert!(!encoded.contains("Alice"));
        assert!(!encoded.contains("accessToken"));
        assert!(!encoded.contains("/Users"));
        assert!(!encoded.contains("-Xmx"));
        assert!(!encoded.contains("raw-secret"));
    }

    #[test]
    fn watchdog_outcome_and_snapshot_notice_are_application_authored() {
        let watchdog = super::launch_session_outcome(LaunchSessionExitReason::WatchdogKilled);
        assert_eq!(watchdog.summary, "Guardian stopped a stalled startup.");
        assert_ne!(
            watchdog,
            LaunchSessionOutcome::from_reason(LaunchSessionExitReason::WatchdogKilled)
        );

        let guardian = warning_summary();
        let record = LaunchSessionRecord {
            session_id: SessionId("notice-parity".to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.5".to_string(),
            launched_at: None,
            benchmark: None,
            state: LaunchState::Exited,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: Some(-1),
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            crash_evidence: None,
            healing: None,
            guardian: Some(serde_json::to_value(&guardian).expect("Guardian JSON")),
            outcome: Some(watchdog),
            stages: Vec::new(),
        };
        let factual = axial_launcher::snapshot_status(&record);
        assert!(factual.notice.is_none());
        let enriched = super::launch_status_snapshot(&record);
        assert_eq!(
            enriched.notice,
            super::launch_notice_from_values(
                record.guardian.as_ref(),
                record.healing.as_ref(),
                record.outcome.as_ref(),
                None,
                None,
            )
        );
    }

    #[test]
    fn admission_projection_is_central_and_preserves_public_lines() {
        let user_outcome = GuardianUserOutcome::authored(
            GuardianActionKind::Warn,
            OperationPhase::Validating,
            "Guardian flagged launch settings for review.".to_string(),
            vec!["Review the selected Java runtime.".to_string()],
            vec!["Keep the current JVM preset.".to_string()],
        );
        let admission = GuardianLaunchAdmission::Preflight {
            safety: SafetyOutcome {
                decision: GuardianActionKind::Warn,
                summary: user_outcome.summary.clone(),
                detail: user_outcome.details.first().cloned(),
                diagnoses: Vec::new(),
            },
            user_outcome,
        };

        let summary = super::guardian_summary_from_admission(GuardianMode::Custom, &admission);

        assert_eq!(summary.mode, GuardianMode::Custom);
        assert_eq!(summary.decision, GuardianSummaryDecision::Warned);
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian flagged launch settings for review.")
        );
        assert_eq!(
            summary.details,
            [
                "Review the selected Java runtime.",
                "Keep the current JVM preset."
            ]
        );
        assert_eq!(summary.guidance, summary.details);
        assert!(summary.interventions.is_empty());
    }

    #[test]
    fn typed_directives_author_all_intervention_details_and_raw_evidence() {
        let cases = [
            (
                crate::guardian::GuardianDirective::UseManagedJava {
                    reason: GuardianManagedJavaReason::PrepareFailure,
                },
                GuardianInterventionKind::SwitchManagedRuntime,
                "Guardian switched to managed Java before launch",
                "Guardian used the managed Java runtime for this launch.",
            ),
            (
                crate::guardian::GuardianDirective::StripJvmArgs {
                    reason: GuardianStripJvmArgsReason::PrepareFailure,
                },
                GuardianInterventionKind::StripJvmArgs,
                "Guardian removed incompatible explicit JVM args before launch",
                "Explicit JVM args were removed before launch because they were incompatible.",
            ),
            (
                crate::guardian::GuardianDirective::compatibility_preset_downgrade(
                    "graalvm",
                    "performance",
                ),
                GuardianInterventionKind::DowngradePreset,
                "Guardian downgraded JVM preset from \"graalvm\" to \"performance\" before launch",
                "JVM preset changed from GraalVM to Performance for compatibility.",
            ),
            (
                crate::guardian::GuardianDirective::startup_preset_downgrade("performance"),
                GuardianInterventionKind::DowngradePreset,
                "Automatic retry: downgraded JVM preset to \"performance\" after startup failure",
                "JVM preset changed to Performance for compatibility.",
            ),
            (
                crate::guardian::GuardianDirective::DisableCustomGc,
                GuardianInterventionKind::DisableCustomGc,
                "Automatic retry: disabled custom GC flags after startup failure",
                "Custom GC flags were disabled for compatibility.",
            ),
        ];

        for (directive, kind, raw_detail, public_detail) in cases {
            let summary =
                super::guardian_summary_with_intervention(&warning_summary(), &directive, false);
            assert_eq!(summary.decision, GuardianSummaryDecision::Intervened);
            assert_eq!(
                summary.message.as_deref(),
                Some("Guardian adjusted launch settings for safety.")
            );
            assert_eq!(
                summary.details,
                [public_detail, "Keep existing launch guidance."]
            );
            assert_eq!(summary.guidance, ["Keep existing launch guidance."]);
            assert_eq!(summary.interventions.len(), 1);
            assert_eq!(summary.interventions[0].kind, kind);
            assert_eq!(summary.interventions[0].detail.as_deref(), Some(raw_detail));
            assert_eq!(
                summary.interventions[0].public_detail.as_deref(),
                Some(public_detail)
            );
            assert_eq!(summary.interventions[0].silent, Some(false));
        }
    }

    #[test]
    fn intervention_projection_preserves_order_dedup_and_silent_behavior() {
        let managed = crate::guardian::GuardianDirective::UseManagedJava {
            reason: GuardianManagedJavaReason::PrepareFailure,
        };
        let stripped = crate::guardian::GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::PrepareFailure,
        };
        let once = super::guardian_summary_with_intervention(&warning_summary(), &managed, false);
        let repeated = super::guardian_summary_with_intervention(&once, &managed, false);
        let expanded = super::guardian_summary_with_intervention(&repeated, &stripped, false);

        assert_eq!(
            expanded.details,
            [
                "Guardian used the managed Java runtime for this launch.",
                "Explicit JVM args were removed before launch because they were incompatible.",
                "Keep existing launch guidance.",
            ]
        );
        assert_eq!(expanded.guidance, ["Keep existing launch guidance."]);
        assert_eq!(expanded.interventions.len(), 3);

        let silent = super::guardian_summary_with_intervention(&warning_summary(), &managed, true);
        assert_eq!(silent.details, ["Keep existing launch guidance."]);
        assert_eq!(silent.guidance, ["Keep existing launch guidance."]);
        assert_eq!(silent.interventions.len(), 1);
        assert_eq!(silent.interventions[0].silent, Some(true));
        assert_eq!(
            silent.interventions[0].detail.as_deref(),
            Some("Guardian switched to managed Java before launch")
        );
        assert_eq!(
            silent.interventions[0].public_detail.as_deref(),
            Some("Guardian used the managed Java runtime for this launch.")
        );
    }

    #[test]
    fn intervention_projection_discards_unrepresented_details_before_bounded_composition() {
        let mut current = warning_summary();
        current.details = vec![
            "Runtime repair-only detail one.".to_string(),
            "Runtime repair-only detail two.".to_string(),
            "Runtime repair-only detail three.".to_string(),
            "Runtime repair-only detail four.".to_string(),
            "Runtime repair-only detail five.".to_string(),
            "Runtime repair-only detail six.".to_string(),
        ];
        let directive = crate::guardian::GuardianDirective::StripJvmArgs {
            reason: GuardianStripJvmArgsReason::PrepareFailure,
        };

        let projected = super::guardian_summary_with_intervention(&current, &directive, false);

        assert_eq!(
            projected.details,
            [
                "Explicit JVM args were removed before launch because they were incompatible.",
                "Keep existing launch guidance.",
            ]
        );
        assert!(
            projected
                .details
                .iter()
                .all(|detail| !detail.contains("Runtime repair-only"))
        );
    }

    #[test]
    fn jvm_preset_copy_table_is_unique_complete_and_bounded() {
        assert_eq!(
            GUARDIAN_JVM_PRESET_COPY_RULES.len(),
            GuardianJvmPresetId::ALL.len()
        );
        for (index, rule) in GUARDIAN_JVM_PRESET_COPY_RULES.iter().enumerate() {
            assert_eq!(rule.preset, GuardianJvmPresetId::ALL[index]);
            assert!(
                GUARDIAN_JVM_PRESET_COPY_RULES[index + 1..]
                    .iter()
                    .all(|other| other.preset != rule.preset)
            );
            assert!(!rule.label.is_empty() && rule.label.len() <= MAX_SUMMARY_BYTES);
            assert!(!rule.detail.is_empty() && rule.detail.len() <= MAX_LINE_BYTES);
        }
    }

    #[test]
    fn install_persistence_projects_only_sealed_decision_summary_and_detail() {
        let outcome = author_guardian_copy(GuardianCopyRequest::install_failure_replay(
            DiagnosisId::ManagedRuntimeRosettaRequired,
            GuardianActionKind::Block,
        ))
        .expect("Rosetta install copy rule");

        assert_eq!(
            guardian_install_outcome_persistence_facts(&outcome),
            vec![
                "guardian_outcome_decision:block".to_string(),
                "guardian_outcome_summary:This Minecraft version needs Rosetta 2 on Apple Silicon Macs."
                    .to_string(),
                "guardian_outcome_detail:Java runtime component the required runtime needs Rosetta 2 on this Mac."
                    .to_string(),
            ]
        );
    }

    #[test]
    fn copy_rule_table_is_unique_complete_and_bounded() {
        assert_eq!(GUARDIAN_COPY_RULES.len(), 27);
        for (index, rule) in GUARDIAN_COPY_RULES.iter().enumerate() {
            assert!(
                GUARDIAN_COPY_RULES[index + 1..]
                    .iter()
                    .all(|other| other.key != rule.key),
                "duplicate copy rule at {index}"
            );
        }

        let mut counts = [0_usize; 5];
        for rule in GUARDIAN_COPY_RULES {
            let index = match rule.key.context {
                CopyContextKey::RuntimeRepaired
                | CopyContextKey::RuntimeBlocked
                | CopyContextKey::RuntimeFailed => 0,
                CopyContextKey::ArtifactRepaired
                | CopyContextKey::ArtifactBlocked
                | CopyContextKey::ArtifactFailed => 1,
                CopyContextKey::InstallFailure => 2,
                CopyContextKey::PerformanceUnsafeOwnership
                | CopyContextKey::PerformanceMissingJournal
                | CopyContextKey::PerformanceUnsafePublicBoundary
                | CopyContextKey::PerformanceGuardianBlocked
                | CopyContextKey::PerformanceFallbackUnavailable
                | CopyContextKey::PerformanceRollbackUnavailable => 3,
                CopyContextKey::PersistedStateLoad => 4,
            };
            counts[index] += 1;
            assert!(rule.summary.len() <= MAX_SUMMARY_BYTES);
            assert!(rule.details.len() <= MAX_COLLECTION_LINES);
            assert!(rule.guidance.len() <= MAX_COLLECTION_LINES);
        }
        assert_eq!(counts, [3, 3, 14, 6, 1]);
    }

    #[test]
    fn preflight_copy_tables_are_unique_and_closed() {
        assert_eq!(PREFLIGHT_SUMMARY_RULES.len(), 7);
        for (index, rule) in PREFLIGHT_SUMMARY_RULES.iter().enumerate() {
            assert!(
                PREFLIGHT_SUMMARY_RULES[index + 1..]
                    .iter()
                    .all(|other| other.decision != rule.decision)
            );
            assert!(rule.summary.len() <= MAX_SUMMARY_BYTES);
        }
        assert_eq!(PREFLIGHT_DIAGNOSIS_RULES.len(), 34);
        for (index, rule) in PREFLIGHT_DIAGNOSIS_RULES.iter().enumerate() {
            assert!(
                PREFLIGHT_DIAGNOSIS_RULES[index + 1..]
                    .iter()
                    .all(|other| other.coordinate != rule.coordinate)
            );
        }
        assert_eq!(PREFLIGHT_INVARIANT_DIAGNOSIS_RULES.len(), 13);
        for (index, rule) in PREFLIGHT_INVARIANT_DIAGNOSIS_RULES.iter().enumerate() {
            assert!(!rule.decisions.is_empty());
            for (decision_index, decision) in rule.decisions.iter().enumerate() {
                assert!(!rule.decisions[decision_index + 1..].contains(decision));
            }
            assert!(
                PREFLIGHT_INVARIANT_DIAGNOSIS_RULES[index + 1..]
                    .iter()
                    .all(|other| other.diagnosis_id != rule.diagnosis_id)
            );
            assert!(PREFLIGHT_DIAGNOSIS_RULES.iter().all(|exact| {
                exact.coordinate.diagnosis_id != rule.diagnosis_id
                    || !rule.decisions.contains(&exact.coordinate.decision)
            }));
        }
    }

    #[test]
    fn preflight_copy_accepts_only_the_boundary_adapter_decision_pair() {
        assert!(
            author_guardian_copy(GuardianCopyRequest::preflight(
                GuardianActionKind::Warn,
                GuardianActionKind::Warn,
                OperationPhase::Validating,
                &[],
                &[],
            ))
            .is_some()
        );
        assert!(
            author_guardian_copy(GuardianCopyRequest::preflight(
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
                OperationPhase::Validating,
                &[],
                &[],
            ))
            .is_some()
        );

        for (authored, effective) in [
            (GuardianActionKind::Block, GuardianActionKind::AskUser),
            (GuardianActionKind::Fallback, GuardianActionKind::Block),
            (GuardianActionKind::AskUser, GuardianActionKind::Warn),
        ] {
            assert!(
                author_guardian_copy(GuardianCopyRequest::preflight(
                    authored,
                    effective,
                    OperationPhase::Validating,
                    &[],
                    &[],
                ))
                .is_none(),
                "accepted invalid {authored:?} -> {effective:?} preflight pair"
            );
        }
    }

    #[test]
    fn preflight_supporting_copy_survives_stronger_verdicts_in_diagnosis_order() {
        let cases = [
            (
                GuardianActionKind::Block,
                GuardianActionKind::Block,
                DiagnosisId::LaunchResourceMemoryPressure,
                "Guardian blocked launch preflight.",
                "Launch memory budget is tight",
            ),
            (
                GuardianActionKind::Fallback,
                GuardianActionKind::Fallback,
                DiagnosisId::LaunchResourceCpuPressure,
                "Guardian adjusted launch preflight.",
                "Launch concurrency may be tight",
            ),
            (
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
                DiagnosisId::LaunchResourceDiskPressure,
                "Guardian needs confirmation before launch.",
                "Launch-relevant storage has low free space",
            ),
            (
                GuardianActionKind::Block,
                GuardianActionKind::Block,
                DiagnosisId::CustomJavaOverridePresent,
                "Guardian blocked launch preflight.",
                "Guardian Custom mode will keep the selected Java override",
            ),
            (
                GuardianActionKind::AskUser,
                GuardianActionKind::Block,
                DiagnosisId::CustomJvmArgsPresent,
                "Guardian needs confirmation before launch.",
                "Guardian Custom mode will keep explicit JVM args",
            ),
        ];
        for (authored, effective, diagnosis, summary, detail) in cases {
            let outcome = author_guardian_copy(GuardianCopyRequest::preflight(
                authored,
                effective,
                OperationPhase::Validating,
                &[diagnosis],
                &[],
            ))
            .expect("supported preflight coordinate");

            assert_eq!(outcome.decision, effective);
            assert_eq!(outcome.summary, summary);
            assert!(outcome.details[0].contains(detail));
        }

        let ordered = author_guardian_copy(GuardianCopyRequest::preflight(
            GuardianActionKind::Fallback,
            GuardianActionKind::Fallback,
            OperationPhase::Validating,
            &[
                DiagnosisId::JavaOverrideUnavailable,
                DiagnosisId::LaunchResourceMemoryPressure,
                DiagnosisId::ManagedRuntimeMissing,
            ],
            &[],
        ))
        .expect("mixed fallback copy");
        assert!(ordered.details[0].contains("unavailable Java override"));
        assert!(ordered.details[1].contains("memory budget is tight"));
        assert!(ordered.details[2].contains("Managed Java runtime is missing"));
    }

    #[test]
    fn hostile_dynamic_install_fields_are_redacted_and_byte_bounded() {
        let evidence = [GuardianInstallArtifactFailureEvidence::launcher_managed(
            None,
            "artifact",
            GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
        )
        .with_field("component", "/home/alice/java --accessToken secret")
        .with_field("component", "ignored-second-value")
        .with_field("platform", "界".repeat(64))];
        let outcome = author_guardian_copy(GuardianCopyRequest::install_failure(
            DiagnosisId::ManagedRuntimeUnavailableForPlatform,
            GuardianActionKind::Block,
            &evidence,
        ))
        .expect("runtime unavailable copy rule");
        let encoded = serde_json::to_string(&outcome).expect("outcome JSON");

        assert_eq!(
            outcome.details,
            ["Java runtime component the required runtime is not available for this device."]
        );
        assert!(outcome.summary.len() <= MAX_SUMMARY_BYTES);
        assert!(
            outcome
                .details
                .iter()
                .chain(&outcome.guidance)
                .all(|line| line.len() <= MAX_LINE_BYTES)
        );
        for sensitive in ["/home", "alice", "accessToken", "secret", "ignored-second"] {
            assert!(
                !encoded.contains(sensitive),
                "leaked {sensitive}: {encoded}"
            );
        }
    }

    #[test]
    fn install_dynamics_use_the_first_matching_evidence_and_field() {
        let evidence = [
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "first",
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", "jre-first")
            .with_field("component", "ignored-field")
            .with_field("platform", "platform-first"),
            GuardianInstallArtifactFailureEvidence::launcher_managed(
                None,
                "second",
                GuardianInstallArtifactFailureKind::RuntimeUnavailableForPlatform,
            )
            .with_field("component", "jre-second")
            .with_field("platform", "platform-second"),
        ];

        let outcome = author_guardian_copy(GuardianCopyRequest::install_failure(
            DiagnosisId::ManagedRuntimeUnavailableForPlatform,
            GuardianActionKind::Block,
            &evidence,
        ))
        .expect("runtime unavailable copy rule");

        assert_eq!(
            outcome.details,
            ["Java runtime component jre-first is not available for platform-first."]
        );
    }

    #[test]
    fn unsupported_copy_coordinate_returns_none() {
        assert_eq!(
            author_guardian_copy(GuardianCopyRequest::runtime_repair(
                Some(DiagnosisId::PersistedStateSchemaInvalid),
                GuardianRepairStatus::Repaired,
            )),
            None
        );
    }

    #[test]
    fn performance_rejection_preserves_rolling_back_phase() {
        let outcome = author_guardian_copy(GuardianCopyRequest::performance_rejection(
            GuardianPerformanceSupervisionRejection::RollbackUnavailable,
            OperationPhase::RollingBack,
        ))
        .expect("performance rejection copy rule");

        assert_eq!(outcome.decision, GuardianActionKind::Block);
        assert_eq!(outcome.phase, OperationPhase::RollingBack);
    }

    #[test]
    fn line_finalization_deduplicates_stably_and_caps_collections() {
        let values = finalize_lines([
            "first".to_string(),
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
            "fourth".to_string(),
            "fifth".to_string(),
            "sixth".to_string(),
            "seventh".to_string(),
        ]);
        assert_eq!(
            values,
            ["first", "second", "third", "fourth", "fifth", "sixth"]
        );
    }

    #[test]
    fn runtime_repair_composition_bounds_and_redacts_prior_copy() {
        let mut prior = empty_summary(GuardianMode::Managed);
        prior.details = vec![
            "/home/alice/private/runtime".to_string(),
            "é".repeat(121),
            "existing detail".to_string(),
            "existing detail".to_string(),
            "second detail".to_string(),
            "third detail".to_string(),
            "fourth detail".to_string(),
            "fifth detail".to_string(),
            "sixth detail".to_string(),
        ];
        prior.guidance = prior.details.clone();

        for status in [GuardianRepairStatus::Repaired, GuardianRepairStatus::Failed] {
            let copy =
                GuardianRuntimeRepairCopy::author(Some(DiagnosisId::ManagedRuntimeCorrupt), status)
                    .expect("runtime repair copy");
            let summary = copy.guardian_summary(&prior);
            assert!(summary.details.len() <= MAX_COLLECTION_LINES);
            assert!(summary.guidance.len() <= MAX_COLLECTION_LINES);
            let encoded = serde_json::to_string(&summary).expect("summary JSON");
            assert!(!encoded.contains("/home"));
            assert!(!encoded.contains("alice"));
            assert!(!encoded.contains(&"é".repeat(121)));
            assert!(
                summary
                    .details
                    .iter()
                    .all(|line| line.len() <= MAX_LINE_BYTES)
            );
            assert!(
                summary
                    .guidance
                    .iter()
                    .all(|line| line.len() <= MAX_LINE_BYTES)
            );
        }
    }

    #[test]
    fn artifact_repair_summary_projects_repaired_and_terminal_states() {
        let current = warning_summary();
        for (status, expected_decision) in [
            (
                GuardianArtifactRepairStatus::Repaired,
                GuardianSummaryDecision::Intervened,
            ),
            (
                GuardianArtifactRepairStatus::Blocked,
                GuardianSummaryDecision::Blocked,
            ),
            (
                GuardianArtifactRepairStatus::Failed,
                GuardianSummaryDecision::Blocked,
            ),
        ] {
            let outcome = author_guardian_copy(GuardianCopyRequest::artifact_repair(
                DiagnosisId::LauncherManagedArtifactCorrupt,
                status,
            ))
            .expect("artifact repair copy");
            let projected =
                super::guardian_summary_with_artifact_repair_outcome(&current, &outcome);
            assert_eq!(projected.decision, expected_decision);
            assert_eq!(projected.message.as_deref(), Some(outcome.summary()));
            assert!(
                projected
                    .details
                    .iter()
                    .all(|detail| detail.len() <= MAX_LINE_BYTES)
            );
        }
    }

    #[test]
    fn suppressed_recovery_preserves_visible_intervention_detail_order() {
        let current = super::guardian_summary_with_intervention(
            &empty_summary(GuardianMode::Managed),
            &crate::guardian::GuardianDirective::compatibility_preset_downgrade(
                "graalvm",
                "performance",
            ),
            false,
        );
        let outcome = super::author_launch_recovery_suppressed_copy(
            &crate::guardian::GuardianDirective::StripJvmArgs {
                reason: crate::guardian::GuardianStripJvmArgsReason::PrepareFailure,
            },
        );

        let projected = super::guardian_summary_with_suppressed_outcome(&current, &outcome);

        assert_eq!(projected.details[0], outcome.summary());
        assert_eq!(
            projected.details[1],
            "JVM preset changed from GraalVM to Performance for compatibility."
        );
        assert_eq!(projected.details[2], outcome.guidance()[0]);
        assert_eq!(projected.interventions, current.interventions);
    }
}
