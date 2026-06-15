use super::LaunchHealingSummary;
use crate::guardian::{GuardianDecision, GuardianSummary};
use crate::healing::HealingEventKind;
use crate::process::{
    LaunchNotice, LaunchNoticeTone, LaunchSessionOutcome, LaunchSessionOutcomeKind,
    LaunchStatusEvent,
};
use crate::types::{LaunchFailureClass, LaunchState};
use serde_json::Value;

pub fn launch_state_name(state: LaunchState) -> &'static str {
    match state {
        LaunchState::Idle => "idle",
        LaunchState::Queued => "queued",
        LaunchState::Planning => "planning",
        LaunchState::Validating => "validating",
        LaunchState::EnsuringRuntime => "ensuring_runtime",
        LaunchState::DownloadingRuntime => "downloading_runtime",
        LaunchState::Preparing => "preparing",
        LaunchState::Prewarming => "prewarming",
        LaunchState::Starting => "starting",
        LaunchState::Monitoring => "monitoring",
        LaunchState::Running => "running",
        LaunchState::Degraded => "degraded",
        LaunchState::Failed => "failed",
        LaunchState::Exited => "exited",
    }
}

pub fn launch_stage_label(stage: &str) -> &'static str {
    match stage {
        "idle" => "Idle",
        "queued" => "Queued",
        "planning" => "Planning launch",
        "validating" => "Validating launch",
        "ensuring_runtime" => "Ensuring runtime",
        "downloading_runtime" => "Downloading runtime",
        "preparing" => "Preparing files",
        "prewarming" => "Prewarming game data",
        "starting" => "Starting process",
        "monitoring" => "Monitoring startup",
        "running" => "Running",
        "degraded" => "Degraded",
        "failed" => "Failed",
        "exited" => "Exited",
        _ => "Launch stage",
    }
}

pub fn is_terminal_status(status: &LaunchStatusEvent) -> bool {
    matches!(status.state.as_str(), "failed" | "exited")
}

pub fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

pub fn failure_class_name(class: LaunchFailureClass) -> &'static str {
    class.as_str()
}

pub fn format_failure_class(class: LaunchFailureClass) -> &'static str {
    match class {
        LaunchFailureClass::Unknown => "unknown startup failure",
        LaunchFailureClass::JvmUnsupportedOption => "unsupported JVM option",
        LaunchFailureClass::JvmExperimentalUnlock => "experimental JVM option requires unlock",
        LaunchFailureClass::JvmOptionOrdering => "JVM option ordering conflict",
        LaunchFailureClass::JavaRuntimeMismatch => "Java runtime mismatch",
        LaunchFailureClass::ClasspathModuleConflict => "classpath or module conflict",
        LaunchFailureClass::AuthModeIncompatible => "auth mode incompatibility",
        LaunchFailureClass::LoaderBootstrapFailure => "loader bootstrap failure",
        LaunchFailureClass::StartupStalled => "startup stalled",
    }
}

pub fn launch_notice_from_values(
    guardian: Option<&Value>,
    healing: Option<&Value>,
    outcome: Option<&LaunchSessionOutcome>,
    lead_detail: Option<&str>,
    fallback_message: Option<&str>,
) -> Option<LaunchNotice> {
    let guardian = guardian.and_then(|value| serde_json::from_value(value.clone()).ok());
    let healing = healing.and_then(|value| serde_json::from_value(value.clone()).ok());
    launch_notice(
        guardian.as_ref(),
        healing.as_ref(),
        outcome,
        lead_detail,
        fallback_message,
    )
}

pub fn launch_notice(
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
            GuardianDecision::Blocked | GuardianDecision::Warned | GuardianDecision::Intervened
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

    let message = guardian_message(guardian)
        .or_else(|| healing_message(healing))
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

    Some(LaunchNotice {
        message,
        detail: details.first().cloned(),
        details,
        tone: launch_notice_tone(guardian, healing, outcome),
    })
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
        push_notice_detail(&mut details, intervention.detail.as_deref());
    }
    for guidance in &guardian.guidance {
        push_notice_detail(&mut details, Some(guidance));
    }
    details
}

fn guardian_message(guardian: Option<&GuardianSummary>) -> Option<String> {
    let guardian = guardian?;
    if let Some(message) = guardian
        .message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Some(message.to_string());
    }
    match guardian.decision {
        GuardianDecision::Blocked => Some("Guardian blocked an unsafe launch setup.".to_string()),
        GuardianDecision::Warned => Some("Guardian found launch settings to review.".to_string()),
        GuardianDecision::Intervened => {
            Some("Guardian adjusted launch settings for safety.".to_string())
        }
        GuardianDecision::Allowed => None,
    }
}

fn healing_message(healing: Option<&LaunchHealingSummary>) -> Option<String> {
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
                    .map(format_failure_class)
                    .unwrap_or("startup failure")
            )),
        );
    }
    details
}

fn healing_event_detail(event: &crate::healing::HealingEvent) -> &str {
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
            .unwrap_or("Croopor retried startup with safer settings."),
    }
}

fn launch_notice_tone(
    guardian: Option<&GuardianSummary>,
    healing: Option<&LaunchHealingSummary>,
    outcome: Option<&LaunchSessionOutcome>,
) -> LaunchNoticeTone {
    match guardian.map(|guardian| guardian.decision) {
        Some(GuardianDecision::Blocked) => return LaunchNoticeTone::Error,
        Some(GuardianDecision::Warned) => return LaunchNoticeTone::Warned,
        Some(GuardianDecision::Intervened) => return LaunchNoticeTone::Intervened,
        Some(GuardianDecision::Allowed) | None => {}
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

pub fn snapshot_status(
    record: &crate::process::LaunchSessionRecord,
) -> crate::process::LaunchStatusEvent {
    crate::process::LaunchStatusEvent {
        state: launch_state_name(record.state).to_string(),
        benchmark: record.benchmark.clone(),
        pid: record.pid,
        exit_code: record.exit_code,
        failure_class: record
            .failure
            .as_ref()
            .map(|failure| failure_class_name(failure.class).to_string()),
        failure_detail: record
            .failure
            .as_ref()
            .and_then(|failure| failure.detail.clone()),
        healing: record.healing.clone(),
        guardian: record.guardian.clone(),
        outcome: record.outcome.clone(),
        notice: launch_notice_from_values(
            record.guardian.as_ref(),
            record.healing.as_ref(),
            record.outcome.as_ref(),
            None,
            None,
        ),
        evidence: Vec::new(),
        stages: record.stages.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian::{GuardianDecision, GuardianMode, GuardianSummary};
    use crate::process::{LaunchSessionExitReason, LaunchSessionOutcome};
    use serde_json::json;

    #[test]
    fn launch_notice_prefers_guardian_authored_block_over_healing() {
        let mut guardian = GuardianSummary::new(GuardianMode::Managed);
        guardian.decision = GuardianDecision::Blocked;
        guardian.message = Some("Guardian blocked an unsafe launch setup.".to_string());
        guardian
            .details
            .push("Custom Java path is unavailable.".to_string());

        let healing = LaunchHealingSummary {
            failure_class: Some("java_runtime_mismatch".to_string()),
            warnings: vec!["Healing warning should not lead.".to_string()],
            ..Default::default()
        };

        let notice = launch_notice(
            Some(&guardian),
            Some(&healing),
            None,
            Some("Fallback detail"),
            Some("Fallback message"),
        )
        .expect("Guardian-authored block should create a notice");

        assert_eq!(
            notice.message,
            "Guardian blocked an unsafe launch setup.".to_string()
        );
        assert_eq!(notice.tone, LaunchNoticeTone::Error);
        assert_eq!(
            notice.details,
            vec!["Custom Java path is unavailable.".to_string()]
        );
        assert!(
            !notice
                .details
                .iter()
                .any(|detail| detail.contains("Java runtime mismatch"))
        );
    }

    #[test]
    fn launch_notice_uses_healing_failure_when_guardian_does_not_own_issue() {
        let healing = LaunchHealingSummary {
            failure_class: Some("java_runtime_mismatch".to_string()),
            ..Default::default()
        };

        let notice = launch_notice(
            None,
            Some(&healing),
            None,
            Some("Java check failed"),
            Some("Fallback message"),
        )
        .expect("Healing failure should create a notice");

        assert_eq!(
            notice.message,
            "Launch stopped before startup because the required Java runtime was not available."
                .to_string()
        );
        assert_eq!(notice.tone, LaunchNoticeTone::Error);
        assert_eq!(notice.detail, Some("Java check failed.".to_string()));
        assert!(
            notice
                .details
                .contains(&"Reason: Java runtime mismatch.".to_string())
        );
    }

    #[test]
    fn launch_notice_suppresses_clean_external_close_without_warning() {
        let outcome =
            LaunchSessionOutcome::from_reason(LaunchSessionExitReason::ExternalUserClosed);

        assert!(launch_notice(None, None, Some(&outcome), None, None).is_none());
    }

    #[test]
    fn launch_notice_surfaces_failed_session_outcome() {
        let outcome = LaunchSessionOutcome::from_reason(LaunchSessionExitReason::CrashedBeforeBoot);

        let notice = launch_notice(None, None, Some(&outcome), None, None)
            .expect("Failed session outcome should create a notice");

        assert_eq!(
            notice.message,
            "Minecraft exited before startup completed.".to_string()
        );
        assert_eq!(notice.tone, LaunchNoticeTone::Error);
        assert_eq!(
            notice.details,
            vec!["Minecraft exited before startup completed.".to_string()]
        );
    }

    #[test]
    fn launch_notice_from_values_accepts_backend_serialized_guardian_summary() {
        let guardian = json!({
            "mode": "managed",
            "decision": "warned",
            "message": "Guardian found launch settings to review.",
            "details": ["Review custom JVM arguments."]
        });

        let notice = launch_notice_from_values(Some(&guardian), None, None, None, None)
            .expect("Serialized Guardian summary should create a notice");

        assert_eq!(
            notice.message,
            "Guardian found launch settings to review.".to_string()
        );
        assert_eq!(notice.tone, LaunchNoticeTone::Warned);
        assert_eq!(
            notice.details,
            vec!["Review custom JVM arguments.".to_string()]
        );
    }
}
