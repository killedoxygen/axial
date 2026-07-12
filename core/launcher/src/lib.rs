pub mod build;
pub mod crash;
pub mod failure;
pub mod guardian;
pub mod healing;
pub mod jvm;
pub mod process;
pub mod profile;
pub mod readiness;
pub mod runtime;
pub mod service;
pub mod session;
pub mod types;

pub use build::{
    LaunchAuthContext, VanillaLaunchPlan, VanillaLaunchPlanError, VanillaLaunchRequest,
    cleanup_natives_dir, plan_resolved_launch, plan_vanilla_launch,
};
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
    PRESET_SMOOTH, PRESET_ULTRA_LOW_LATENCY, boot_throttle_args, gc_preset_args,
    recommended_preset, sanitize_preset,
};
pub use process::{
    LaunchEvent, LaunchLogEvent, LaunchNotice, LaunchNoticeTone, LaunchPriorityEvidence,
    LaunchSessionExitReason, LaunchSessionOutcome, LaunchSessionOutcomeKind, LaunchSessionRecord,
    LaunchStageEvidence, LaunchStageRecord, LaunchStatusEvent,
};
pub use readiness::{
    LaunchReadiness, LaunchReadinessReason, LaunchReadinessReasonId, LaunchReadinessRequest,
    LaunchReadinessSeverity, inspect_launch_readiness, inspect_launch_readiness_summary,
};
pub use runtime::RuntimeSelection;
pub use service::{
    LaunchHealingSummary, LaunchIntent, LaunchPreparationError, LaunchPreparationEvent,
    PreparedLaunchAttempt, build_healing_summary, failure_class_name, format_failure_class,
    is_terminal_state, is_terminal_status, launch_stage_label, launch_state_name,
    prepare_launch_attempt_with_events, sanitize_effective_runtime_major, snapshot_status,
};
pub use types::{InstanceId, LaunchFailure, LaunchFailureClass, LaunchState, SessionId, VersionId};
