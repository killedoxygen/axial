use croopor_launcher::{
    LaunchFailureClass, LaunchSessionExitReason, LaunchSessionOutcome, LaunchState,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct SessionOutcomeInput {
    pub previous_state: LaunchState,
    pub next_state: LaunchState,
    pub boot_completed: bool,
    pub stop_requested: bool,
    pub exit_code: Option<i32>,
    pub failure_class: Option<LaunchFailureClass>,
}

pub(super) fn parse_launch_state(state: &str) -> LaunchState {
    match state {
        "queued" => LaunchState::Queued,
        "planning" => LaunchState::Planning,
        "validating" => LaunchState::Validating,
        "ensuring_runtime" => LaunchState::EnsuringRuntime,
        "downloading_runtime" => LaunchState::DownloadingRuntime,
        "preparing" => LaunchState::Preparing,
        "prewarming" => LaunchState::Prewarming,
        "starting" => LaunchState::Starting,
        "monitoring" => LaunchState::Monitoring,
        "running" => LaunchState::Running,
        "degraded" => LaunchState::Degraded,
        "failed" => LaunchState::Failed,
        "exited" => LaunchState::Exited,
        _ => LaunchState::Idle,
    }
}

pub(super) fn is_terminal_state(state: LaunchState) -> bool {
    matches!(state, LaunchState::Failed | LaunchState::Exited)
}

pub(super) fn parse_failure_class(raw: &str) -> LaunchFailureClass {
    LaunchFailureClass::from_name(raw).unwrap_or(LaunchFailureClass::Unknown)
}

pub(super) fn classify_session_outcome(input: SessionOutcomeInput) -> Option<LaunchSessionOutcome> {
    if !is_terminal_state(input.next_state) {
        return None;
    }

    let reason = if input.stop_requested {
        LaunchSessionExitReason::LauncherStopped
    } else if input.failure_class == Some(LaunchFailureClass::StartupStalled) {
        LaunchSessionExitReason::StartupStalled
    } else if input.boot_completed {
        classify_post_boot_outcome(input)
    } else {
        classify_pre_boot_outcome(input)
    };

    Some(LaunchSessionOutcome::from_reason(reason))
}

fn classify_post_boot_outcome(input: SessionOutcomeInput) -> LaunchSessionExitReason {
    if input.exit_code == Some(0) && input.failure_class.is_none() {
        if matches!(
            input.previous_state,
            LaunchState::Running | LaunchState::Degraded
        ) {
            LaunchSessionExitReason::ExternalUserClosed
        } else {
            LaunchSessionExitReason::CleanExit
        }
    } else if input.exit_code.is_some_and(|code| code != 0) || input.failure_class.is_some() {
        LaunchSessionExitReason::CrashedAfterBoot
    } else {
        LaunchSessionExitReason::UnknownExit
    }
}

fn classify_pre_boot_outcome(input: SessionOutcomeInput) -> LaunchSessionExitReason {
    match input.failure_class {
        Some(LaunchFailureClass::Unknown) | None => {
            if input.exit_code.is_some_and(|code| code != 0) {
                LaunchSessionExitReason::CrashedBeforeBoot
            } else {
                LaunchSessionExitReason::UnknownExit
            }
        }
        Some(_) => LaunchSessionExitReason::StartupFailed,
    }
}

pub(super) fn boot_marker_detected(text: &str) -> bool {
    const BOOT_MARKERS: [&str; 3] = ["Setting user:", "LWJGL Version", "Minecraft Launcher"];
    BOOT_MARKERS.iter().any(|marker| text.contains(marker)) || render_thread_atlas_created(text)
}

fn render_thread_atlas_created(text: &str) -> bool {
    text.contains("[Render thread")
        && text.contains("Created:")
        && (text.contains("textures/atlas/") || text.contains("-atlas"))
}

#[cfg(test)]
mod tests {
    use super::{SessionOutcomeInput, boot_marker_detected, classify_session_outcome};
    use croopor_launcher::{LaunchFailureClass, LaunchSessionExitReason, LaunchState};

    #[test]
    fn boot_marker_detected_accepts_explicit_boot_evidence() {
        let accepted = [
            "[Client thread/INFO]: Setting user: Player",
            "[Render thread/INFO]: LWJGL Version: 3.3.3",
            "[main/INFO]: Minecraft Launcher 1.6.93",
            "[Render thread/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas",
            "[Render thread/INFO]: Created: 256x256x0 minecraft:textures/atlas/particles.png-atlas",
        ];

        for line in accepted {
            assert!(boot_marker_detected(line), "{line}");
        }
    }

    #[test]
    fn boot_marker_detected_rejects_generic_render_thread_logs() {
        let rejected = [
            "[Render thread/INFO]: Reloading ResourceManager: vanilla",
            "[Render thread/INFO]: OpenGL debug message: id=1280",
            "[Render thread/INFO]: Created renderer",
            "[Worker-Main-1/INFO]: Created: 1024x512x4 minecraft:textures/atlas/blocks.png-atlas",
        ];

        for line in rejected {
            assert!(!boot_marker_detected(line), "{line}");
        }
    }

    #[test]
    fn session_outcome_classifies_clean_external_close_after_boot() {
        let outcome = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Running,
            next_state: LaunchState::Exited,
            boot_completed: true,
            stop_requested: false,
            exit_code: Some(0),
            failure_class: None,
        })
        .expect("outcome");

        assert_eq!(outcome.reason, LaunchSessionExitReason::ExternalUserClosed);
    }

    #[test]
    fn session_outcome_classifies_launcher_stop_separately() {
        let outcome = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Running,
            next_state: LaunchState::Exited,
            boot_completed: true,
            stop_requested: true,
            exit_code: Some(-9),
            failure_class: None,
        })
        .expect("outcome");

        assert_eq!(outcome.reason, LaunchSessionExitReason::LauncherStopped);
    }

    #[test]
    fn session_outcome_classifies_startup_stall_and_preboot_crash() {
        let stalled = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Monitoring,
            next_state: LaunchState::Exited,
            boot_completed: false,
            stop_requested: false,
            exit_code: Some(-1),
            failure_class: Some(LaunchFailureClass::StartupStalled),
        })
        .expect("stalled outcome");
        let crashed = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Starting,
            next_state: LaunchState::Exited,
            boot_completed: false,
            stop_requested: false,
            exit_code: Some(1),
            failure_class: None,
        })
        .expect("crashed outcome");

        assert_eq!(stalled.reason, LaunchSessionExitReason::StartupStalled);
        assert_eq!(crashed.reason, LaunchSessionExitReason::CrashedBeforeBoot);
    }

    #[test]
    fn session_outcome_classifies_startup_failure_postboot_crash_and_unknown_exit() {
        let startup_failed = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Starting,
            next_state: LaunchState::Exited,
            boot_completed: false,
            stop_requested: false,
            exit_code: Some(1),
            failure_class: Some(LaunchFailureClass::JvmUnsupportedOption),
        })
        .expect("startup failure outcome");
        let post_boot_crash = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Running,
            next_state: LaunchState::Exited,
            boot_completed: true,
            stop_requested: false,
            exit_code: Some(1),
            failure_class: None,
        })
        .expect("post boot crash outcome");
        let unknown = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Starting,
            next_state: LaunchState::Exited,
            boot_completed: false,
            stop_requested: false,
            exit_code: Some(0),
            failure_class: None,
        })
        .expect("unknown outcome");

        assert_eq!(
            startup_failed.reason,
            LaunchSessionExitReason::StartupFailed
        );
        assert_eq!(
            post_boot_crash.reason,
            LaunchSessionExitReason::CrashedAfterBoot
        );
        assert_eq!(unknown.reason, LaunchSessionExitReason::UnknownExit);
    }

    #[test]
    fn session_outcome_supports_clean_exit_reason() {
        let outcome = classify_session_outcome(SessionOutcomeInput {
            previous_state: LaunchState::Monitoring,
            next_state: LaunchState::Exited,
            boot_completed: true,
            stop_requested: false,
            exit_code: Some(0),
            failure_class: None,
        })
        .expect("outcome");

        assert_eq!(outcome.reason, LaunchSessionExitReason::CleanExit);
    }
}
