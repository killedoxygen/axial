use croopor_launcher::{LaunchFailureClass, LaunchState};

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
    use super::boot_marker_detected;

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
}
