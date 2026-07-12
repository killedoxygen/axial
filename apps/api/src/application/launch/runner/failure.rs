use super::LaunchRequestError;
use super::proof::persist_launch_proof_with_context_owned as persist_launch_proof_with_context;
use super::status::{serialize_guardian, serialize_healing};
use crate::guardian::GuardianSummary;
use crate::observability::{
    RedactionAudience, sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::launch_reports::LaunchProofContext;
use crate::state::{AppState, LaunchStatusEvent};
use axial_launcher::{LaunchFailureClass, LaunchSessionOutcome, failure_class_name};

const LIVE_LAUNCH_FAILURE_MAX_CHARS: usize = 180;
const LIVE_LAUNCH_FAILURE_SAFE_FALLBACK: &str = "Launch failed before Minecraft could start. Detailed diagnostics were hidden because they may contain local paths or private data.";
// must match prepare.rs's `resolve java: {error}` wrapping of
// JavaRuntimeLookupError::RosettaRequired, pinned by a test
const ROSETTA_REQUIRED_LAUNCH_MESSAGE_PREFIX: &str = "resolve java: java runtime ";
const ROSETTA_REQUIRED_LAUNCH_MESSAGE_SUFFIX: &str = " needs Rosetta 2 on this Mac: run `softwareupdate --install-rosetta --agree-to-license` in Terminal";

pub(super) struct LaunchFailure<'a> {
    pub(super) proof_context: Option<&'a LaunchProofContext>,
    pub(super) class: LaunchFailureClass,
    pub(super) message: &'a str,
    pub(super) healing: Option<axial_launcher::LaunchHealingSummary>,
    pub(super) guardian: Option<GuardianSummary>,
    pub(super) outcome: Option<LaunchSessionOutcome>,
}

pub(super) async fn fail_launch(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    failure: LaunchFailure<'_>,
) -> LaunchRequestError {
    let public_message = sanitize_live_launch_failure_message(failure.message);
    emit_terminal_failure(
        state,
        session_id,
        TerminalFailureEvent {
            state: "exited",
            class: failure.class,
            message: &public_message,
            healing: failure.healing.clone(),
            guardian: failure.guardian.clone(),
            outcome: failure.outcome,
        },
    )
    .await;
    persist_launch_proof_with_context(
        state,
        producer,
        session_id,
        None,
        "failed",
        failure.proof_context,
    )
    .await;
    LaunchRequestError {
        message: public_message,
        healing: failure.healing,
        guardian: failure.guardian,
    }
}

pub(super) async fn fail_launch_for_journal(
    state: &AppState,
    producer: &crate::state::ProducerLease,
    session_id: &str,
    message: &str,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
) -> LaunchRequestError {
    let public_message = sanitize_live_launch_failure_message(message);
    emit_terminal_failure(
        state,
        session_id,
        TerminalFailureEvent {
            state: "failed",
            class: LaunchFailureClass::Unknown,
            message: &public_message,
            healing: healing.clone(),
            guardian: guardian.clone(),
            outcome: None,
        },
    )
    .await;
    persist_launch_proof_with_context(state, producer, session_id, None, "failed", None).await;
    LaunchRequestError {
        message: public_message,
        healing,
        guardian,
    }
}

pub fn sanitize_live_launch_failure_message(message: &str) -> String {
    // the Rosetta hint's `--` flags trip the sensitive-text heuristic, so
    // rebuild a trusted message instead of losing the guidance
    if let Some(public_message) = rosetta_required_launch_failure_message(message) {
        return public_message;
    }
    sanitize_public_diagnostic_text(
        message,
        RedactionAudience::UserVisible,
        LIVE_LAUNCH_FAILURE_MAX_CHARS,
        LIVE_LAUNCH_FAILURE_SAFE_FALLBACK,
    )
}

fn rosetta_required_launch_failure_message(message: &str) -> Option<String> {
    let component = message
        .strip_prefix(ROSETTA_REQUIRED_LAUNCH_MESSAGE_PREFIX)?
        .strip_suffix(ROSETTA_REQUIRED_LAUNCH_MESSAGE_SUFFIX)?;
    let component = sanitize_evidence_token(component, RedactionAudience::UserVisible, 64)?;
    Some(format!(
        "This Minecraft version needs Rosetta 2 on Apple Silicon Macs. Required runtime: {component}. Install Rosetta 2 by running `softwareupdate --install-rosetta --agree-to-license` in Terminal, then launch again."
    ))
}

struct TerminalFailureEvent<'a> {
    state: &'static str,
    class: LaunchFailureClass,
    message: &'a str,
    healing: Option<axial_launcher::LaunchHealingSummary>,
    guardian: Option<GuardianSummary>,
    outcome: Option<LaunchSessionOutcome>,
}

async fn emit_terminal_failure(
    state: &AppState,
    session_id: &str,
    event: TerminalFailureEvent<'_>,
) {
    state
        .sessions()
        .emit_log(session_id, "system", event.message.to_string())
        .await;
    state
        .sessions()
        .emit_status(
            session_id,
            LaunchStatusEvent {
                state: event.state.to_string(),
                benchmark: None,
                pid: None,
                exit_code: Some(-1),
                failure_class: Some(failure_class_name(event.class).to_string()),
                failure_detail: Some(event.message.to_string()),
                crash_evidence: None,
                healing: serialize_healing(event.healing),
                guardian: serialize_guardian(event.guardian),
                outcome: event.outcome,
                notice: None,
                evidence: Vec::new(),
                stages: Vec::new(),
            },
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, LaunchEvent, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_launcher::{LaunchSessionExitReason, LaunchSessionRecord, LaunchState, SessionId};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rosetta_required_launch_message_keeps_guidance_and_pins_core_display() {
        // built from the real core error so a Display change fails here
        // instead of silently regressing to the generic fallback
        let error = axial_minecraft::JavaRuntimeLookupError::RosettaRequired {
            component: "jre-legacy".to_string(),
        };
        let message = format!("resolve java: {error}");

        let public_message = sanitize_live_launch_failure_message(&message);

        assert!(public_message.contains("Rosetta 2"));
        assert!(public_message.contains("jre-legacy"));
        assert!(public_message.contains("softwareupdate --install-rosetta --agree-to-license"));
        assert!(!public_message.contains("Detailed diagnostics were hidden"));
    }

    #[test]
    fn rosetta_recognizer_rejects_tampered_component_tokens() {
        let message = format!(
            "{ROSETTA_REQUIRED_LAUNCH_MESSAGE_PREFIX}/home/alice/.axial/evil{ROSETTA_REQUIRED_LAUNCH_MESSAGE_SUFFIX}"
        );

        let public_message = sanitize_live_launch_failure_message(&message);

        assert!(!public_message.contains("/home/alice"));
        assert!(public_message.contains("Launch failed before Minecraft could start"));
    }

    #[test]
    fn other_flag_bearing_launch_messages_still_fall_back_to_generic() {
        let public_message =
            sanitize_live_launch_failure_message("spawn failed: java --username SecretPlayer");

        assert!(public_message.contains("Launch failed before Minecraft could start"));
    }

    #[tokio::test]
    async fn fail_launch_sanitizes_public_error_and_terminal_failure_payloads() {
        let root = unique_test_dir("live-launch-failure");
        let state = test_app_state(&root);
        let session_id = "unsafe-live-failure";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");
        let mut events = state
            .sessions()
            .subscribe(session_id)
            .await
            .expect("subscribe");
        let unsafe_message = "prepare failed for /home/alice/.axial/instances/secret java.exe --accessToken raw-secret-token -Xmx8192M -Dtoken=raw provider_payload=provider-secret account_id=account-secret username=SecretPlayer\nnext command fragment C:\\Users\\Alice\\AppData\\java.exe eyJheader123456789.abcdEFGH12345678.ijklMNOP12345678";
        let producer = state.try_claim_producer().expect("claim failure producer");

        let error = fail_launch(
            &state,
            &producer,
            session_id,
            LaunchFailure {
                proof_context: None,
                class: LaunchFailureClass::Unknown,
                message: unsafe_message,
                healing: None,
                guardian: None,
                outcome: None,
            },
        )
        .await;

        assert_safe_live_launch_failure_text(&error.message);
        assert!(
            error
                .message
                .contains("Launch failed before Minecraft could start")
        );

        let log_event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("log event")
            .expect("log event result");
        let log_text = match log_event {
            LaunchEvent::Log(log) => log.text,
            other => panic!("expected log event, got {other:?}"),
        };
        assert_safe_live_launch_failure_text(&log_text);
        assert!(log_text.contains("Detailed diagnostics were hidden"));

        let status_event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("status event")
            .expect("status event result");
        let failure_detail = match status_event {
            LaunchEvent::Status(status) => {
                assert_eq!(status.state, "exited");
                status.failure_detail.expect("failure detail")
            }
            other => panic!("expected status event, got {other:?}"),
        };
        assert_safe_live_launch_failure_text(&failure_detail);
        assert!(failure_detail.contains("Detailed diagnostics were hidden"));

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal failure session record");
        assert_eq!(record.state, LaunchState::Exited);
        assert!(record.failure.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fail_launch_records_spawn_failure_in_session_and_proof() {
        let root = unique_test_dir("spawn-failed-outcome");
        let state = test_app_state(&root);
        let session_id = "spawn-failed-outcome";
        state
            .sessions()
            .insert(test_record(session_id))
            .await
            .expect("insert session");

        let expected = LaunchSessionOutcome::from_reason(LaunchSessionExitReason::SpawnFailed);
        let producer = state.try_claim_producer().expect("claim failure producer");
        let _ = fail_launch(
            &state,
            &producer,
            session_id,
            LaunchFailure {
                proof_context: None,
                class: LaunchFailureClass::Unknown,
                message: "failed to start launch process: program not found",
                healing: None,
                guardian: None,
                outcome: Some(expected.clone()),
            },
        )
        .await;

        let record = state
            .sessions()
            .get(session_id)
            .await
            .expect("terminal failure session record");
        assert_eq!(record.outcome.as_ref(), Some(&expected));

        let proof = state
            .launch_reports()
            .load(session_id)
            .expect("proof exists");
        assert_eq!(proof.session_outcome.as_ref(), Some(&expected));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_launch_failure_sanitizer_keeps_safe_bounded_errors_useful() {
        let message = "launch plan did not produce a runnable command after preparation completed";

        let sanitized = sanitize_live_launch_failure_message(message);

        assert_eq!(sanitized, message);
        assert_safe_live_launch_failure_text(&sanitized);
    }

    fn test_app_state(root: &Path) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn test_record(session_id: &str) -> LaunchSessionRecord {
        LaunchSessionRecord {
            session_id: SessionId(session_id.to_string()),
            instance_id: "instance".to_string(),
            version_id: "1.21.1".to_string(),
            launched_at: Some("2026-01-01T00:00:00.000Z".to_string()),
            benchmark: None,
            state: LaunchState::Queued,
            pid: None,
            process_started_at_ms: None,
            boot_completed_at_ms: None,
            boot_duration_ms: None,
            priority: None,
            exit_code: None,
            command: Vec::new(),
            java_path: None,
            natives_dir: None,
            failure: None,
            crash_evidence: None,
            healing: None,
            guardian: None,
            outcome: None,
            stages: Vec::new(),
        }
    }

    fn assert_safe_live_launch_failure_text(text: &str) {
        assert!(text.chars().count() <= LIVE_LAUNCH_FAILURE_MAX_CHARS + 3);
        assert!(!text.contains('\n'));
        assert!(!text.contains('\r'));
        for fragment in [
            "/home/alice",
            "C:\\Users",
            "--accessToken",
            "-Xmx8192M",
            "-Dtoken",
            "raw-secret",
            "provider_payload",
            "provider-secret",
            "account_id",
            "account-secret",
            "username",
            "SecretPlayer",
            "java.exe",
            "eyJheader123456789",
        ] {
            assert!(
                !text.contains(fragment),
                "live launch failure leaked fragment {fragment:?}: {text}"
            );
        }
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
