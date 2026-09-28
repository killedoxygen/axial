//! Bounded command-shape inspection. Raw command material never enters this API.

use serde::Serialize;

pub const MAX_INSPECTED_ARGUMENTS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaunchCommandInspection {
    pub session_id: String,
    pub command: Vec<&'static str>,
    pub command_redacted: bool,
    pub command_arg_count: usize,
    pub java_path_present: bool,
}

/// `session_id` and counts come from the session owner's retained command summary,
/// never a request-authored command. Large commands preserve their actual count
/// while the rendered placeholder list remains bounded.
pub fn inspect_launch_command(
    session_id: &str,
    command_arg_count: usize,
    java_path_present: bool,
) -> LaunchCommandInspection {
    LaunchCommandInspection {
        session_id: session_id.to_owned(),
        command: vec!["<redacted>"; command_arg_count.min(MAX_INSPECTED_ARGUMENTS)],
        command_redacted: command_arg_count > 0,
        command_arg_count,
        java_path_present,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_session_summary_serializes_without_command_material() {
        let summary = inspect_launch_command("550e8400-e29b-41d4-a716-446655440000", 3, true);
        assert_eq!(
            serde_json::to_value(summary).expect("serialize command inspection"),
            serde_json::json!({
                "session_id": "550e8400-e29b-41d4-a716-446655440000",
                "command": ["<redacted>", "<redacted>", "<redacted>"],
                "command_redacted": true,
                "command_arg_count": 3,
                "java_path_present": true
            })
        );
    }

    #[test]
    fn empty_command_is_not_redacted() {
        let summary = inspect_launch_command("session-empty", 0, false);
        assert!(summary.command.is_empty());
        assert!(!summary.command_redacted);
        assert!(!summary.java_path_present);
    }

    #[test]
    fn large_command_has_bounded_output_and_exact_original_count() {
        let summary = inspect_launch_command("session-large", 1_000_000, true);
        assert_eq!(summary.command.len(), MAX_INSPECTED_ARGUMENTS);
        assert_eq!(summary.command_arg_count, 1_000_000);
        assert!(summary.command.iter().all(|value| *value == "<redacted>"));
    }
}
