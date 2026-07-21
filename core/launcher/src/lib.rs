pub mod build;
pub mod crash;
pub mod failure;
pub mod guardian;
pub mod healing;
pub mod jvm;
pub mod process;
pub mod readiness;
pub mod runtime;
pub mod service;
pub mod types;

pub use build::{LaunchAuthContext, VanillaLaunchPlan};
pub use crash::{
    CRASH_ARTIFACT_EXIT_CORRELATION_WINDOW_MS, CrashArtifactKind, CrashEvidence,
    CrashExceptionClass, CrashFailurePhase, CrashModEvidence, CrashModName, CrashModVersion,
    CrashNativeFrameKind, CrashNativeModule, CrashNativeSymbol, CrashProblematicFrame,
    MAX_CRASH_ARTIFACT_BYTES, parse_crash_evidence,
};
pub use failure::{classify_launch_failure, classify_startup_failure_text};
pub use guardian::{
    GuardianMode, LAUNCH_DISK_HEADROOM_MB, LAUNCH_MEMORY_HEADROOM_MB, LaunchGuardianContext,
    OverrideOrigin,
};
pub use healing::{HealingEvent, HealingEventKind};
pub use jvm::{
    PRESET_GRAALVM, PRESET_LEGACY, PRESET_LEGACY_HEAVY, PRESET_LEGACY_PVP, PRESET_PERFORMANCE,
    PRESET_SMOOTH, PRESET_ULTRA_LOW_LATENCY,
};
pub use process::{
    LaunchEvent, LaunchLogEvent, LaunchNotice, LaunchNoticeTone, LaunchPriorityEvidence,
    LaunchSessionExitReason, LaunchSessionOutcome, LaunchSessionOutcomeKind, LaunchSessionRecord,
    LaunchStageEvidence, LaunchStageRecord, LaunchStatusEvent, RevisionedLaunchStatus,
};
pub use readiness::{
    LaunchReadiness, LaunchReadinessReason, LaunchReadinessReasonId, LaunchReadinessRequest,
    LaunchReadinessSeverity, inspect_launch_readiness_structural, inspect_launch_readiness_summary,
};
pub use runtime::RuntimeSelection;
#[cfg(feature = "test-support")]
pub use service::prepare_launch_attempt_with_persisted_runtime_manifest_for_test;
pub use service::{
    LaunchHealingSummary, LaunchIntent, LaunchPreparationError, LaunchPreparationEvent,
    PreparedLaunchAttempt, build_healing_summary, failure_class_name, format_failure_class,
    launch_stage_label, launch_state_name, prepare_launch_attempt_with_events, snapshot_status,
};
pub use types::{LaunchFailure, LaunchFailureClass, LaunchState, SessionId};
