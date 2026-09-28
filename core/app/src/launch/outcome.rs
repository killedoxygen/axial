//! Neutral launch evidence and terminal outcome classification.
//! No repair, retry, relaunch, or process ownership is granted by an outcome.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Unknown,
    JvmUnsupportedOption,
    JvmExperimentalUnlock,
    JvmOptionOrdering,
    JavaRuntimeMismatch,
    RosettaRequired,
    OutOfMemory,
    GraphicsDriverCrash,
    MissingDependency,
    ModTransformationFailure,
    ModAttributedCrash,
    ClasspathModuleConflict,
    LauncherManagedArtifactSignature,
    AuthModeIncompatible,
    LoaderBootstrapFailure,
    StartupStalled,
}

/// Captured evidence contains no command arguments, paths, credentials, or raw output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchEvidence {
    pub boot_observed: bool,
    pub failure_classes: Vec<FailureClass>,
}

impl LaunchEvidence {
    pub fn observe_line(&mut self, line: &str) {
        self.boot_observed |= boot_marker_detected(line);
        self.observe_failure(classify_startup_failure_text(line));
    }

    pub fn observe_failure(&mut self, class: FailureClass) {
        if class != FailureClass::Unknown && !self.failure_classes.contains(&class) {
            self.failure_classes.push(class);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionExitFacts {
    pub process_exited: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stop_requested: bool,
    pub was_running: bool,
    pub tree_settled: bool,
    pub outputs_drained: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcomeKind {
    Clean,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExitReason {
    SpawnFailed,
    CleanExit,
    ExternalUserClosed,
    LauncherStopped,
    StartupStalled,
    StartupFailed,
    CrashedBeforeBoot,
    CrashedAfterBoot,
    UnknownExit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutcome {
    pub kind: SessionOutcomeKind,
    pub reason: SessionExitReason,
    pub failure_class: Option<FailureClass>,
    pub summary: String,
}

impl SessionOutcome {
    /// Used only when the process owner knows spawn did not create a child.
    pub fn spawn_failed() -> Self {
        Self {
            kind: SessionOutcomeKind::Failed,
            reason: SessionExitReason::SpawnFailed,
            failure_class: None,
            summary: "The game process could not be started.".to_owned(),
        }
    }

    pub fn summary(&self) -> &'static str {
        match self.reason {
            SessionExitReason::SpawnFailed => "The game process could not be started.",
            SessionExitReason::CleanExit => "The game closed normally.",
            SessionExitReason::ExternalUserClosed => "The game was closed outside the launcher after startup.",
            SessionExitReason::LauncherStopped => "The game was stopped from the launcher.",
            SessionExitReason::StartupStalled => "Game startup did not complete.",
            SessionExitReason::StartupFailed => "The game could not finish starting.",
            SessionExitReason::CrashedBeforeBoot => "The game exited unexpectedly during startup.",
            SessionExitReason::CrashedAfterBoot => "The game exited unexpectedly after startup.",
            SessionExitReason::UnknownExit => "The game exited without enough evidence to classify the result.",
        }
    }
}

/// Child exit, descendant settlement and output drainage are separate facts.
/// Incomplete settlement must remain visibly nonterminal in the session owner.
pub fn classify_session_outcome(
    facts: SessionExitFacts,
    evidence: &LaunchEvidence,
) -> Option<SessionOutcome> {
    if !facts.process_exited || !facts.tree_settled || !facts.outputs_drained {
        return None;
    }
    let stalled = evidence.failure_classes.contains(&FailureClass::StartupStalled);
    let failure_class = if stalled {
        Some(FailureClass::StartupStalled)
    } else {
        classify_launch_failure(&evidence.failure_classes, facts.exit_code, None)
    };
    let abnormal = facts.signal.is_some() || facts.exit_code.is_some_and(|code| code != 0);
    let (kind, reason) = if stalled {
        (SessionOutcomeKind::Failed, SessionExitReason::StartupStalled)
    } else if facts.stop_requested {
        (SessionOutcomeKind::Stopped, SessionExitReason::LauncherStopped)
    } else if evidence.boot_observed && facts.exit_code == Some(0) && !abnormal {
        (SessionOutcomeKind::Clean, if facts.was_running { SessionExitReason::ExternalUserClosed } else { SessionExitReason::CleanExit })
    } else if evidence.boot_observed && abnormal {
        (SessionOutcomeKind::Failed, SessionExitReason::CrashedAfterBoot)
    } else if !evidence.boot_observed
        && failure_class.is_some_and(|class| class != FailureClass::Unknown)
    {
        (SessionOutcomeKind::Failed, SessionExitReason::StartupFailed)
    } else if abnormal {
        (SessionOutcomeKind::Failed, SessionExitReason::CrashedBeforeBoot)
    } else {
        (SessionOutcomeKind::Unknown, SessionExitReason::UnknownExit)
    };
    let mut outcome = SessionOutcome {
        kind,
        reason,
        failure_class: if kind == SessionOutcomeKind::Failed { failure_class } else { None },
        summary: String::new(),
    };
    outcome.summary = outcome.summary().to_owned();
    Some(outcome)
}

/// Preserve the existing evidence markers; a generic render-thread line is insufficient.
pub fn boot_marker_detected(text: &str) -> bool {
    ["Setting user:", "LWJGL Version", "Minecraft Launcher"]
        .iter()
        .any(|marker| text.contains(marker))
        || (text.contains("[Render thread")
            && text.contains("Created:")
            && (text.contains("textures/atlas/") || text.contains("-atlas")))
}

use super::reports::{
    CrashArtifactKind, CrashEvidence, CrashNativeFrameKind, is_out_of_memory_failure_line,
};

pub fn classify_startup_failure_text(text: &str) -> FailureClass {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return FailureClass::Unknown;
    }
    if lower.contains("unrecognized vm option") || lower.contains("unsupported vm option") {
        return FailureClass::JvmUnsupportedOption;
    }
    if lower.contains("must be enabled via -xx:+unlockexperimentalvmoptions") {
        return FailureClass::JvmExperimentalUnlock;
    }
    if lower.contains("unlock option must precede")
        || lower.contains("unlockexperimentalvmoptions must precede")
        || lower.contains("unlockdiagnosticvmoptions must precede")
    {
        return FailureClass::JvmOptionOrdering;
    }
    if lower.contains("unsupportedclassversionerror")
        || lower.contains("compiled by a more recent version of the java runtime")
        || contains_requires_java_version(&lower)
    {
        return FailureClass::JavaRuntimeMismatch;
    }
    if contains_out_of_memory_failure(&lower) {
        return FailureClass::OutOfMemory;
    }
    if contains_artifact_signature_failure(&lower) {
        return FailureClass::LauncherManagedArtifactSignature;
    }
    if contains_missing_dependency_failure(&lower) {
        return FailureClass::MissingDependency;
    }
    if contains_mod_transformation_failure(&lower) {
        return FailureClass::ModTransformationFailure;
    }
    if lower.contains("resolutionexception: modules")
        || lower.contains("export package")
        || lower.contains("modulelayerhandler.buildlayer")
        || lower.contains("noclassdeffounderror")
        || lower.contains("classnotfoundexception")
        || lower.contains("failed to locate library:")
        || lower.contains("unsatisfiedlinkerror")
    {
        return FailureClass::ClasspathModuleConflict;
    }
    if lower.contains("nosuchelementexception: no value present")
        || (contains_loader_bootstrap_marker(&lower) && contains_failure_context(&lower))
    {
        return FailureClass::LoaderBootstrapFailure;
    }
    if lower.contains("microsoft account")
        || lower.contains("check your microsoft account")
        || lower.contains("multiplayer is disabled")
    {
        return FailureClass::AuthModeIncompatible;
    }
    FailureClass::Unknown
}

pub fn classify_launch_failure(
    stdout_classes: &[FailureClass],
    exit_code: Option<i32>,
    crash_evidence: Option<&CrashEvidence>,
) -> Option<FailureClass> {
    if exit_code == Some(0) {
        return None;
    }

    let stdout_class = stdout_classes
        .iter()
        .copied()
        .filter(|class| *class != FailureClass::StartupStalled)
        .reduce(stronger_failure_class);
    let evidence_class = crash_evidence.and_then(classify_crash_evidence);

    Some(match (stdout_class, evidence_class) {
        (Some(stdout), Some(evidence)) => stronger_failure_class(stdout, evidence),
        (Some(class), None) | (None, Some(class)) => class,
        (None, None) => FailureClass::Unknown,
    })
}

fn classify_crash_evidence(evidence: &CrashEvidence) -> Option<FailureClass> {
    if evidence.names_out_of_memory {
        return Some(FailureClass::OutOfMemory);
    }
    if evidence.source == CrashArtifactKind::JvmFatalError
        && evidence.problematic_frame.as_ref().is_some_and(|frame| {
            frame.kind == CrashNativeFrameKind::Native
                && is_graphics_driver_module(frame.module.as_str())
        })
    {
        return Some(FailureClass::GraphicsDriverCrash);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && evidence
            .exception_class
            .as_ref()
            .is_some_and(|class| is_missing_dependency_exception(class.as_str()))
    {
        return Some(FailureClass::MissingDependency);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && evidence
            .exception_class
            .as_ref()
            .is_some_and(|class| is_mod_transformation_exception(class.as_str()))
    {
        return Some(FailureClass::ModTransformationFailure);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && !evidence.truncated
        && !evidence.suspected_mods.is_empty()
    {
        return Some(FailureClass::ModAttributedCrash);
    }
    None
}

fn stronger_failure_class(
    left: FailureClass,
    right: FailureClass,
) -> FailureClass {
    if failure_class_precedence(left) <= failure_class_precedence(right) {
        left
    } else {
        right
    }
}

fn failure_class_precedence(class: FailureClass) -> u8 {
    match class {
        FailureClass::StartupStalled => 0,
        FailureClass::JvmUnsupportedOption => 1,
        FailureClass::JvmExperimentalUnlock => 2,
        FailureClass::JvmOptionOrdering => 3,
        FailureClass::RosettaRequired => 4,
        FailureClass::JavaRuntimeMismatch => 5,
        FailureClass::OutOfMemory => 6,
        FailureClass::LauncherManagedArtifactSignature => 7,
        FailureClass::GraphicsDriverCrash => 8,
        FailureClass::MissingDependency => 9,
        FailureClass::ModTransformationFailure => 10,
        FailureClass::ModAttributedCrash => 11,
        FailureClass::ClasspathModuleConflict => 12,
        FailureClass::LoaderBootstrapFailure => 13,
        FailureClass::AuthModeIncompatible => 14,
        FailureClass::Unknown => 15,
    }
}

fn is_graphics_driver_module(module: &str) -> bool {
    matches!(
        module.to_ascii_lowercase().as_str(),
        "nvoglv32"
            | "nvoglv64"
            | "nvwgf2um"
            | "nvwgf2umx"
            | "atioglxx"
            | "atio6axx"
            | "amdxx32"
            | "amdxx64"
            | "ig4icd32"
            | "ig4icd64"
            | "ig9icd32"
            | "ig9icd64"
            | "igd10iumd32"
            | "igd10iumd64"
            | "libglx_nvidia"
            | "libnvidia-glcore"
            | "radeonsi_dri"
            | "iris_dri"
            | "i965_dri"
    )
}

fn is_missing_dependency_exception(class: &str) -> bool {
    matches!(
        class,
        "net.minecraftforge.fml.common.MissingModsException"
            | "cpw.mods.fml.common.MissingModsException"
    )
}

fn is_mod_transformation_exception(class: &str) -> bool {
    matches!(
        class,
        "org.spongepowered.asm.mixin.transformer.throwables.MixinApplyError"
            | "org.spongepowered.asm.mixin.transformer.throwables.MixinTransformerError"
            | "org.spongepowered.asm.mixin.transformer.throwables.InvalidMixinException"
            | "org.spongepowered.asm.mixin.injection.throwables.InjectionError"
            | "org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException"
            | "org.spongepowered.asm.mixin.injection.throwables.InjectionValidationException"
    )
}

fn contains_out_of_memory_failure(text: &str) -> bool {
    text.lines().any(is_out_of_memory_failure_line)
}

fn contains_requires_java_version(text: &str) -> bool {
    text.lines().any(|line| {
        ["requires java ", "requires java version "]
            .into_iter()
            .any(|marker| {
                line.find(marker).is_some_and(|index| {
                    line[index + marker.len()..]
                        .trim_start()
                        .chars()
                        .next()
                        .is_some_and(|character| character.is_ascii_digit())
                })
            })
    })
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

fn contains_missing_dependency_failure(text: &str) -> bool {
    text.lines().map(str::trim).any(|line| {
        throwable_line_names(line, "net.minecraftforge.fml.common.missingmodsexception")
            || throwable_line_names(line, "cpw.mods.fml.common.missingmodsexception")
            || line == "missing or unsupported mandatory dependencies:"
            || (line.starts_with("- mod '")
                && line.contains(" requires ")
                && line.ends_with(", which is missing!"))
    })
}

fn contains_mod_transformation_failure(text: &str) -> bool {
    const MIXIN_THROWABLES: [&str; 6] = [
        "org.spongepowered.asm.mixin.transformer.throwables.mixinapplyerror",
        "org.spongepowered.asm.mixin.transformer.throwables.mixintransformererror",
        "org.spongepowered.asm.mixin.transformer.throwables.invalidmixinexception",
        "org.spongepowered.asm.mixin.injection.throwables.injectionerror",
        "org.spongepowered.asm.mixin.injection.throwables.invalidinjectionexception",
        "org.spongepowered.asm.mixin.injection.throwables.injectionvalidationexception",
    ];
    text.lines().map(str::trim).any(|line| {
        MIXIN_THROWABLES
            .iter()
            .any(|class| throwable_line_names(line, class))
            || line.starts_with("failed to load coremod ")
            || line.starts_with("error loading coremod ")
    })
}

fn throwable_line_names(line: &str, class: &str) -> bool {
    line.split_ascii_whitespace()
        .map(|token| token.trim_end_matches(':'))
        .any(|token| token == class)
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
    use super::*;

    fn exit_facts() -> SessionExitFacts {
        SessionExitFacts {
            process_exited: true,
            exit_code: Some(0),
            signal: None,
            stop_requested: false,
            was_running: false,
            tree_settled: true,
            outputs_drained: true,
        }
    }

    #[test]
    fn terminal_outcome_waits_for_process_tree_and_both_output_streams() {
        let evidence = LaunchEvidence { boot_observed: true, ..Default::default() };
        for facts in [
            SessionExitFacts { process_exited: false, ..exit_facts() },
            SessionExitFacts { tree_settled: false, ..exit_facts() },
            SessionExitFacts { outputs_drained: false, ..exit_facts() },
        ] {
            assert!(classify_session_outcome(facts, &evidence).is_none());
        }
        assert_eq!(classify_session_outcome(exit_facts(), &evidence).unwrap().kind, SessionOutcomeKind::Clean);
    }

    #[test]
    fn zero_exit_before_boot_is_unknown_and_stop_is_not_a_crash() {
        assert_eq!(classify_session_outcome(exit_facts(), &LaunchEvidence::default()).unwrap().reason, SessionExitReason::UnknownExit);
        let facts = SessionExitFacts { exit_code: None, signal: Some(9), stop_requested: true, ..exit_facts() };
        assert_eq!(classify_session_outcome(facts, &LaunchEvidence::default()).unwrap().reason, SessionExitReason::LauncherStopped);
        let facts = SessionExitFacts { stop_requested: false, ..facts };
        assert_eq!(classify_session_outcome(facts, &LaunchEvidence::default()).unwrap().reason, SessionExitReason::CrashedBeforeBoot);
    }

    #[test]
    fn explicit_stall_remains_the_reason_when_the_process_is_stopped() {
        let evidence = LaunchEvidence { boot_observed: false, failure_classes: vec![FailureClass::StartupStalled] };
        let facts = SessionExitFacts { stop_requested: true, exit_code: Some(1), ..exit_facts() };
        assert_eq!(classify_session_outcome(facts, &evidence).unwrap().reason, SessionExitReason::StartupStalled);
    }
    use super::super::reports::{MAX_CRASH_ARTIFACT_BYTES, parse_crash_evidence};

    const RANKED_FAILURES: [FailureClass; 16] = [
        FailureClass::StartupStalled,
        FailureClass::JvmUnsupportedOption,
        FailureClass::JvmExperimentalUnlock,
        FailureClass::JvmOptionOrdering,
        FailureClass::RosettaRequired,
        FailureClass::JavaRuntimeMismatch,
        FailureClass::OutOfMemory,
        FailureClass::LauncherManagedArtifactSignature,
        FailureClass::GraphicsDriverCrash,
        FailureClass::MissingDependency,
        FailureClass::ModTransformationFailure,
        FailureClass::ModAttributedCrash,
        FailureClass::ClasspathModuleConflict,
        FailureClass::LoaderBootstrapFailure,
        FailureClass::AuthModeIncompatible,
        FailureClass::Unknown,
    ];

    fn report(raw: &str) -> CrashEvidence {
        parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, raw.as_bytes())
            .expect("Minecraft crash evidence")
    }

    fn hs_err(frame_kind: char, module: &str) -> CrashEvidence {
        let raw = format!("# Problematic frame:\n# {frame_kind}  [{module}.dll+0x12] crash+0x1");
        parse_crash_evidence(CrashArtifactKind::JvmFatalError, raw.as_bytes())
            .expect("JVM fatal-error evidence")
    }

    #[test]
    fn startup_failure_text_classification_is_bounded_to_failure_class() {
        for (output, expected) in [
            (
                "Unrecognized VM option '-XX:+UseZGC' in /home/alice/.axial/instances/secret",
                FailureClass::JvmUnsupportedOption,
            ),
            (
                "java.lang.UnsupportedClassVersionError: compiled by a more recent version of the Java Runtime",
                FailureClass::JavaRuntimeMismatch,
            ),
            (
                "Mod Example requires Java 17 or later",
                FailureClass::JavaRuntimeMismatch,
            ),
            (
                "UnlockExperimentalVMOptions must precede 'UseZGC'",
                FailureClass::JvmOptionOrdering,
            ),
            (
                "Caused by: java.lang.NoClassDefFoundError: org/lwjgl/glfw/GLFW",
                FailureClass::ClasspathModuleConflict,
            ),
            (
                "java.lang.UnsatisfiedLinkError: Failed to locate library: lwjgl.dll",
                FailureClass::ClasspathModuleConflict,
            ),
            (
                "java.lang.SecurityException: class net.minecraft.SomeClass signer information does not match signer information of other classes in the same package",
                FailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "Exception in thread \"main\" java.lang.SecurityException: Invalid signature file digest for Manifest main attributes",
                FailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "java.lang.SecurityException: SHA-256 digest error for net/minecraft/client/Minecraft.class",
                FailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "[main/INFO] [cpw.mods.modlauncher.Launcher/MODLAUNCHER]: ModLauncher running",
                FailureClass::Unknown,
            ),
            (
                "[EARLYDISPLAY/]: If this message is the only thing at the bottom of your log before a crash, you probably have a driver issue.",
                FailureClass::Unknown,
            ),
            (
                "This troubleshooting guide requires Java knowledge",
                FailureClass::Unknown,
            ),
            (
                "The Example mod must precede another mod in the load order",
                FailureClass::Unknown,
            ),
            (
                "cpw.mods.modlauncher.api.IncompatibleEnvironmentException: failed to load transformation service",
                FailureClass::LoaderBootstrapFailure,
            ),
            ("ordinary launcher output", FailureClass::Unknown),
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                expected,
                "{output:?}"
            );
        }
    }

    #[test]
    fn startup_failure_text_classifies_only_exact_memory_failures() {
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
                FailureClass::OutOfMemory,
                "expected OOM classification for {output:?}"
            );
        }
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
                FailureClass::Unknown,
                "unexpected OOM classification for {output:?}"
            );
        }
    }

    #[test]
    fn dependency_and_transformation_text_markers_are_narrow() {
        for output in [
            "Caused by: net.minecraftforge.fml.common.MissingModsException: missing mods",
            "cpw.mods.fml.common.MissingModsException: missing mods",
            "Missing or unsupported mandatory dependencies:",
            "- Mod 'Example' requires library 1.0, which is missing!",
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                FailureClass::MissingDependency,
                "{output:?}"
            );
        }
        for output in [
            "Caused by: org.spongepowered.asm.mixin.transformer.throwables.MixinApplyError: failed",
            "org.spongepowered.asm.mixin.injection.throwables.InjectionError: failed",
            "Failed to load coremod example.CorePlugin",
            "Error loading coremod example.CorePlugin",
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                FailureClass::ModTransformationFailure,
                "{output:?}"
            );
        }
        for output in [
            "Missing dependency documentation loaded",
            "- Mod 'Example' recommends library 1.0, which is missing!",
            "net.minecraftforge.fml.common.MissingModsExceptionHelper: decoy",
            "[main/INFO] Loading mixin configuration example.mixins.json",
            "org.spongepowered.asm.mixin.transformer.MixinTransformer running",
            "Failed to load coremodel example.Model",
        ] {
            assert_eq!(
                classify_startup_failure_text(output),
                FailureClass::Unknown,
                "{output:?}"
            );
        }
    }

    #[test]
    fn failure_precedence_is_total_and_pairwise_stable() {
        for (index, class) in RANKED_FAILURES.iter().copied().enumerate() {
            assert_eq!(failure_class_precedence(class), index as u8);
            assert!(
                !RANKED_FAILURES[..index].contains(&class),
                "duplicate failure class {class:?}"
            );
        }
        for (left_index, left) in RANKED_FAILURES.iter().copied().enumerate() {
            for (right_index, right) in RANKED_FAILURES.iter().copied().enumerate() {
                let expected = if left_index <= right_index {
                    left
                } else {
                    right
                };
                assert_eq!(stronger_failure_class(left, right), expected);
            }
        }
    }

    #[test]
    fn fusion_is_independent_of_candidate_order_and_ignores_lifecycle_state() {
        let mut candidates = RANKED_FAILURES[1..].to_vec();
        candidates.push(FailureClass::StartupStalled);
        candidates.push(FailureClass::JvmUnsupportedOption);
        assert_eq!(
            classify_launch_failure(&candidates, Some(1), None),
            Some(FailureClass::JvmUnsupportedOption)
        );
        candidates.reverse();
        assert_eq!(
            classify_launch_failure(&candidates, Some(1), None),
            Some(FailureClass::JvmUnsupportedOption)
        );
        assert_eq!(
            classify_launch_failure(&[FailureClass::StartupStalled], Some(1), None),
            Some(FailureClass::Unknown)
        );
    }

    #[test]
    fn exit_status_table_distinguishes_clean_and_failed_processes_exactly() {
        let oom =
            report("Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space");
        let semantic = [FailureClass::JvmUnsupportedOption];
        for (exit_code, classes, evidence, expected) in [
            (Some(0), &[][..], None, None),
            (Some(0), &semantic[..], Some(&oom), None),
            (Some(1), &[][..], None, Some(FailureClass::Unknown)),
            (None, &[][..], None, Some(FailureClass::Unknown)),
            (
                Some(-1),
                &semantic[..],
                None,
                Some(FailureClass::JvmUnsupportedOption),
            ),
            (
                None,
                &[][..],
                Some(&oom),
                Some(FailureClass::OutOfMemory),
            ),
        ] {
            assert_eq!(
                classify_launch_failure(classes, exit_code, evidence),
                expected,
                "exit code {exit_code:?}"
            );
        }
    }

    #[test]
    fn structured_evidence_and_stdout_share_the_same_precedence() {
        let stdout = [
            FailureClass::AuthModeIncompatible,
            FailureClass::ClasspathModuleConflict,
        ];
        let cases = [
            (
                report(
                    "Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space\nSuspected Mods: Example Mod (example)",
                ),
                FailureClass::OutOfMemory,
            ),
            (
                report(
                    "Description: Loading game\nnet.minecraftforge.fml.common.MissingModsException: missing\nSuspected Mods: Example Mod (example)",
                ),
                FailureClass::MissingDependency,
            ),
            (
                report(
                    "Description: Loading game\norg.spongepowered.asm.mixin.transformer.throwables.MixinApplyError: failed\nSuspected Mods: Example Mod (example)",
                ),
                FailureClass::ModTransformationFailure,
            ),
            (
                report(
                    "Description: Rendering game\njava.lang.IllegalStateException: failed\nSuspected Mods: Example Mod (example)",
                ),
                FailureClass::ModAttributedCrash,
            ),
        ];
        for (evidence, expected) in cases {
            assert_eq!(
                classify_launch_failure(&stdout, Some(1), Some(&evidence)),
                Some(expected)
            );
        }

        let graphics = hs_err('C', "nvoglv64");
        assert_eq!(
            classify_launch_failure(&stdout, Some(1), Some(&graphics)),
            Some(FailureClass::GraphicsDriverCrash)
        );
        assert_eq!(
            classify_launch_failure(
                &[FailureClass::LauncherManagedArtifactSignature],
                Some(1),
                Some(&graphics),
            ),
            Some(FailureClass::LauncherManagedArtifactSignature)
        );

        let oom =
            report("Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space");
        assert_eq!(
            classify_launch_failure(
                &[FailureClass::JavaRuntimeMismatch],
                Some(1),
                Some(&oom),
            ),
            Some(FailureClass::JavaRuntimeMismatch)
        );
    }

    #[test]
    fn graphics_driver_detection_uses_only_native_frames_and_the_closed_module_table() {
        for module in [
            "nvoglv32",
            "nvoglv64",
            "nvwgf2um",
            "nvwgf2umx",
            "atioglxx",
            "atio6axx",
            "amdxx32",
            "amdxx64",
            "ig4icd32",
            "ig4icd64",
            "ig9icd32",
            "ig9icd64",
            "igd10iumd32",
            "igd10iumd64",
            "libGLX_nvidia",
            "libnvidia-glcore",
            "radeonsi_dri",
            "iris_dri",
            "i965_dri",
        ] {
            assert!(is_graphics_driver_module(module), "{module}");
        }
        for module in [
            "libjvm",
            "opengl32",
            "nvidia",
            "nvoglv64_helper",
            "private-nvoglv64",
        ] {
            assert!(!is_graphics_driver_module(module), "{module}");
        }

        let vm_frame = hs_err('V', "nvoglv64");
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&vm_frame)),
            Some(FailureClass::Unknown)
        );
    }

    #[test]
    fn typed_exception_sets_are_exact() {
        for class in [
            "net.minecraftforge.fml.common.MissingModsException",
            "cpw.mods.fml.common.MissingModsException",
        ] {
            assert!(is_missing_dependency_exception(class));
        }
        for class in [
            "net.minecraftforge.fml.common.MissingModsExceptionHelper",
            "net.fabricmc.loader.impl.discovery.ModResolutionException",
        ] {
            assert!(!is_missing_dependency_exception(class));
        }
        for class in [
            "org.spongepowered.asm.mixin.transformer.throwables.MixinApplyError",
            "org.spongepowered.asm.mixin.transformer.throwables.MixinTransformerError",
            "org.spongepowered.asm.mixin.transformer.throwables.InvalidMixinException",
            "org.spongepowered.asm.mixin.injection.throwables.InjectionError",
            "org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException",
            "org.spongepowered.asm.mixin.injection.throwables.InjectionValidationException",
        ] {
            assert!(is_mod_transformation_exception(class));
        }
        for class in [
            "org.spongepowered.asm.mixin.transformer.MixinTransformer",
            "org.spongepowered.asm.mixin.throwables.MixinException",
            "org.objectweb.asm.ClassTooLargeException",
        ] {
            assert!(!is_mod_transformation_exception(class));
        }
    }

    #[test]
    fn mod_attribution_requires_a_complete_minecraft_report() {
        let mut raw = b"Description: Rendering game\njava.lang.IllegalStateException: failed\nSuspected Mods: Example Mod (example)\n".to_vec();
        raw.resize(MAX_CRASH_ARTIFACT_BYTES + 1, b'x');
        let truncated = parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, &raw)
            .expect("truncated report evidence");
        assert!(truncated.truncated);
        assert!(!truncated.suspected_mods.is_empty());
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&truncated)),
            Some(FailureClass::Unknown)
        );

        let fatal = hs_err('C', "libjvm");
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&fatal)),
            Some(FailureClass::Unknown)
        );
    }

    #[test]
    fn typed_exception_and_native_evidence_require_their_declared_source() {
        let mut missing = report(
            "Description: Loading game\nnet.minecraftforge.fml.common.MissingModsException: missing",
        );
        missing.source = CrashArtifactKind::JvmFatalError;
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&missing)),
            Some(FailureClass::Unknown)
        );

        let mut graphics = hs_err('C', "nvoglv64");
        graphics.source = CrashArtifactKind::MinecraftCrashReport;
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&graphics)),
            Some(FailureClass::Unknown)
        );
    }

    #[test]
    fn truncated_reports_keep_exact_exception_and_oom_evidence_only() {
        let mut raw = b"Description: Loading game\nnet.minecraftforge.fml.common.MissingModsException: missing\n".to_vec();
        raw.resize(MAX_CRASH_ARTIFACT_BYTES + 1, b'x');
        let missing = parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, &raw)
            .expect("truncated missing-dependency evidence");
        assert!(missing.truncated);
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&missing)),
            Some(FailureClass::MissingDependency)
        );

        let mut raw =
            b"Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space\n".to_vec();
        raw.resize(MAX_CRASH_ARTIFACT_BYTES + 1, b'x');
        let oom = parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, &raw)
            .expect("truncated out-of-memory evidence");
        assert!(oom.truncated);
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&oom)),
            Some(FailureClass::OutOfMemory)
        );
    }
}
