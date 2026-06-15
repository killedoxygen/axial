use super::GuardianDecisionKind;
use crate::observability::{RedactionAudience, sanitize_evidence_text};
use crate::state::contracts::OperationPhase;
use serde::{Deserialize, Serialize};

const STARTUP_FAILURE_SUMMARY_FALLBACK: &str = "Guardian blocked launch startup.";
const STARTUP_FAILURE_DETAIL_FALLBACK: &str =
    "Minecraft startup did not complete, and unsafe technical details were hidden.";
const MAX_OUTCOME_SUMMARY_CHARS: usize = 180;
const MAX_OUTCOME_DETAIL_CHARS: usize = 240;
const MAX_OUTCOME_DETAILS: usize = 6;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GuardianUserOutcome {
    pub decision: GuardianDecisionKind,
    pub phase: OperationPhase,
    pub summary: String,
    pub details: Vec<String>,
    pub guidance: Vec<String>,
}

impl GuardianUserOutcome {
    pub fn apply_to_launch_summary(&self, guardian: &mut croopor_launcher::GuardianSummary) {
        if self.decision == GuardianDecisionKind::Block {
            guardian.decision = croopor_launcher::GuardianDecision::Blocked;
            guardian.message = Some(self.summary.clone());
            guardian.details = self.details.clone();
            guardian.guidance = self.guidance.clone();
        }
    }
}

pub fn startup_failure_guardian_outcome(
    decision: &croopor_launcher::StartupFailureDecision,
    existing_guidance: &[String],
) -> GuardianUserOutcome {
    let summary = public_text(&decision.message, MAX_OUTCOME_SUMMARY_CHARS)
        .unwrap_or_else(|| STARTUP_FAILURE_SUMMARY_FALLBACK.to_string());
    let reason = public_text(&decision.reason, MAX_OUTCOME_DETAIL_CHARS)
        .unwrap_or_else(|| STARTUP_FAILURE_DETAIL_FALLBACK.to_string());

    let mut guidance = Vec::new();
    for detail in existing_guidance.iter().chain(decision.guidance.iter()) {
        if let Some(detail) = public_text(detail, MAX_OUTCOME_DETAIL_CHARS) {
            push_unique_bounded(&mut guidance, detail, MAX_OUTCOME_DETAILS);
        }
    }

    let mut details = vec![reason];
    for detail in &guidance {
        push_unique_bounded(&mut details, detail.clone(), MAX_OUTCOME_DETAILS);
    }

    GuardianUserOutcome {
        decision: GuardianDecisionKind::Block,
        phase: OperationPhase::Launching,
        summary,
        details,
        guidance,
    }
}

fn public_text(value: &str, max_chars: usize) -> Option<String> {
    sanitize_evidence_text(value, RedactionAudience::UserVisible, max_chars)
}

fn push_unique_bounded(values: &mut Vec<String>, value: String, max_values: usize) {
    let value = value.trim();
    if value.is_empty()
        || values.len() >= max_values
        || values.iter().any(|existing| existing == value)
    {
        return;
    }
    values.push(value.to_string());
}

#[cfg(test)]
mod tests {
    use super::{STARTUP_FAILURE_DETAIL_FALLBACK, startup_failure_guardian_outcome};
    use crate::guardian::GuardianDecisionKind;
    use croopor_launcher::{LaunchFailureClass, StartupFailureDecision};

    #[test]
    fn startup_failure_outcome_orders_reason_existing_guidance_and_new_guidance() {
        let decision = StartupFailureDecision {
            class: LaunchFailureClass::StartupStalled,
            message: "Guardian blocked launch startup.".to_string(),
            reason: "No startup activity was observed before the startup window ended.".to_string(),
            guidance: vec![
                "Review the latest game log before retrying.".to_string(),
                "Launch memory budget is tight.".to_string(),
            ],
        };
        let existing = vec!["Launch memory budget is tight.".to_string()];

        let outcome = startup_failure_guardian_outcome(&decision, &existing);

        assert_eq!(outcome.decision, GuardianDecisionKind::Block);
        assert_eq!(outcome.summary, "Guardian blocked launch startup.");
        assert_eq!(
            outcome.details,
            vec![
                "No startup activity was observed before the startup window ended.",
                "Launch memory budget is tight.",
                "Review the latest game log before retrying.",
            ]
        );
        assert_eq!(
            outcome.guidance,
            vec![
                "Launch memory budget is tight.",
                "Review the latest game log before retrying.",
            ]
        );
    }

    #[test]
    fn startup_failure_outcome_redacts_unsafe_public_text() {
        let decision = StartupFailureDecision {
            class: LaunchFailureClass::Unknown,
            message: "/home/alice/.minecraft/java.exe --accessToken secret".to_string(),
            reason: r#"C:\Users\Alice\AppData\java.exe -Xmx8192M --username Alice {"provider":"payload"}"#
                .to_string(),
            guidance: vec![
                "token-shaped abcdefgh.ijklmnop.qrstuvwx".to_string(),
                "account_id=123 username=alice".to_string(),
                "Review the latest game log before retrying.".to_string(),
            ],
        };

        let outcome = startup_failure_guardian_outcome(&decision, &[]);
        let encoded = serde_json::to_string(&outcome).expect("outcome json");
        let lower = encoded.to_ascii_lowercase();

        assert_eq!(outcome.summary, "Guardian blocked launch startup.");
        assert_eq!(outcome.details[0], STARTUP_FAILURE_DETAIL_FALLBACK);
        assert!(
            outcome
                .guidance
                .contains(&"Review the latest game log before retrying.".to_string())
        );
        assert!(!lower.contains("/home"));
        assert!(!lower.contains("users\\\\alice"));
        assert!(!lower.contains("-xmx"));
        assert!(!lower.contains("--username"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("account_id"));
        assert!(!lower.contains("provider"));
    }

    #[test]
    fn startup_failure_outcome_applies_to_existing_launch_summary_shape() {
        let decision = StartupFailureDecision {
            class: LaunchFailureClass::JvmUnsupportedOption,
            message: "Guardian blocked launch startup.".to_string(),
            reason: "Minecraft exited before startup completed with a detected JVM option compatibility failure."
                .to_string(),
            guidance: vec![
                "Remove the explicit JVM args or switch Guardian Mode back to Managed.".to_string(),
            ],
        };
        let mut summary =
            croopor_launcher::GuardianSummary::new(croopor_launcher::GuardianMode::Custom);

        startup_failure_guardian_outcome(&decision, &[]).apply_to_launch_summary(&mut summary);

        assert_eq!(
            summary.decision,
            croopor_launcher::GuardianDecision::Blocked
        );
        assert_eq!(
            summary.message.as_deref(),
            Some("Guardian blocked launch startup.")
        );
        assert_eq!(
            summary.details,
            vec![
                "Minecraft exited before startup completed with a detected JVM option compatibility failure.",
                "Remove the explicit JVM args or switch Guardian Mode back to Managed.",
            ]
        );
    }
}
