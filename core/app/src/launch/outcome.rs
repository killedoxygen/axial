//! Neutral launch evidence and terminal outcome classification.
//! No repair, retry, relaunch, or process ownership is granted by an outcome.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Unknown,
    JvmUnsupportedOption,
    JvmExperimentalUnlock,
    JvmOptionOrdering,
    JavaRuntimeMismatch,
    RosettaRequired,
    OutOfMemory,
    GraphicsUnavailable,
    MissingDependency,
    ModTransformationFailure,
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
    pub(super) graphics_api_unavailable: bool,
    pub(super) graphics_window_creation_failed: bool,
}

impl LaunchEvidence {
    pub fn observe_line(&mut self, line: &str) {
        self.boot_observed |= boot_marker_detected(line);
        self.observe_failure(classify_startup_failure_text(line));
        self.graphics_api_unavailable |= line.trim() == "[LWJGL] GLFW_API_UNAVAILABLE error";
        self.graphics_window_creation_failed |= is_uncaught_glfw_window_failure(line);
        if self.graphics_api_unavailable && self.graphics_window_creation_failed {
            self.observe_failure(FailureClass::GraphicsUnavailable);
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcomeKind {
    Clean,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
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
        if self.reason == SessionExitReason::StartupFailed
            && self.failure_class == Some(FailureClass::GraphicsUnavailable)
        {
            return "The game could not create a window because OpenGL is unavailable.";
        }
        if self.reason == SessionExitReason::StartupFailed
            && self.failure_class == Some(FailureClass::MissingDependency)
        {
            return "A mod needs another mod or loader version that isn't installed, so the game could not start.";
        }
        match self.reason {
            SessionExitReason::SpawnFailed => "The game process could not be started.",
            SessionExitReason::CleanExit => "The game closed normally.",
            SessionExitReason::ExternalUserClosed => {
                "The game was closed outside the launcher after startup."
            }
            SessionExitReason::LauncherStopped => "The game was stopped from the launcher.",
            SessionExitReason::StartupStalled => "Game startup did not complete.",
            SessionExitReason::StartupFailed => "The game could not finish starting.",
            SessionExitReason::CrashedBeforeBoot => "The game exited unexpectedly during startup.",
            SessionExitReason::CrashedAfterBoot => "The game exited unexpectedly after startup.",
            SessionExitReason::UnknownExit => {
                "The game exited without enough evidence to classify the result."
            }
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
    let stalled = evidence
        .failure_classes
        .contains(&FailureClass::StartupStalled);
    let failure_class = if stalled {
        Some(FailureClass::StartupStalled)
    } else {
        classify_launch_failure(&evidence.failure_classes, facts.exit_code)
    };
    let abnormal = facts.signal.is_some() || facts.exit_code.is_some_and(|code| code != 0);
    let (kind, reason) = if stalled {
        (
            SessionOutcomeKind::Failed,
            SessionExitReason::StartupStalled,
        )
    } else if facts.stop_requested
        && !(!evidence.boot_observed && failure_class == Some(FailureClass::MissingDependency))
    {
        (
            SessionOutcomeKind::Stopped,
            SessionExitReason::LauncherStopped,
        )
    } else if facts.stop_requested {
        // Fabric keeps the JVM alive on its incompatible-mods dialog, so this
        // stop ends a start that had already failed.
        (SessionOutcomeKind::Failed, SessionExitReason::StartupFailed)
    } else if failure_class == Some(FailureClass::GraphicsUnavailable) {
        (SessionOutcomeKind::Failed, SessionExitReason::StartupFailed)
    } else if evidence.boot_observed && facts.exit_code == Some(0) && !abnormal {
        (
            SessionOutcomeKind::Clean,
            if facts.was_running {
                SessionExitReason::ExternalUserClosed
            } else {
                SessionExitReason::CleanExit
            },
        )
    } else if evidence.boot_observed && abnormal {
        (
            SessionOutcomeKind::Failed,
            SessionExitReason::CrashedAfterBoot,
        )
    } else if !evidence.boot_observed
        && failure_class.is_some_and(|class| class != FailureClass::Unknown)
    {
        (SessionOutcomeKind::Failed, SessionExitReason::StartupFailed)
    } else if abnormal {
        (
            SessionOutcomeKind::Failed,
            SessionExitReason::CrashedBeforeBoot,
        )
    } else {
        (SessionOutcomeKind::Unknown, SessionExitReason::UnknownExit)
    };
    let mut outcome = SessionOutcome {
        kind,
        reason,
        failure_class: if kind == SessionOutcomeKind::Failed {
            failure_class
        } else {
            None
        },
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

use super::crash::is_out_of_memory_failure_line;

pub fn classify_startup_failure_text(text: &str) -> FailureClass {
    if is_graphics_unavailable_failure(text) {
        return FailureClass::GraphicsUnavailable;
    }
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

fn is_graphics_unavailable_failure(text: &str) -> bool {
    let text = text.trim();
    // Minecraft 26.x aborts the JVM when its render thread has no usable OpenGL context.
    if let Some((thread, message)) = text
        .strip_prefix("FATAL ERROR in native method: [LWJGL] Thread[")
        .and_then(|rest| rest.split_once("]: "))
    {
        return thread.contains("Render thread")
            && message.starts_with(
                "No context is current or a function that is not available in the current context was called.",
            );
    }
    let Some(throwable) = text.strip_prefix("<log4j:Throwable><![CDATA[") else {
        return false;
    };
    let Some((class, message)) = throwable.split_once(": ") else {
        return false;
    };
    !class.is_empty()
        && class
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '$' | '_'))
        && message
            .strip_suffix("]]></log4j:Throwable>")
            .unwrap_or(message)
            == "GLFW error 65542: WGL: The driver does not appear to support OpenGL"
}

fn is_uncaught_glfw_window_failure(text: &str) -> bool {
    let text = text.trim();
    let text = text
        .strip_prefix("<log4j:Message><![CDATA[")
        .and_then(|text| text.strip_suffix("]]></log4j:Message>"))
        .unwrap_or(text);
    let Some((line, message)) = text
        .strip_prefix("[java.lang.ThreadGroup:uncaughtException:")
        .and_then(|text| text.split_once("]: "))
    else {
        return false;
    };
    !line.is_empty()
        && line.bytes().all(|value| value.is_ascii_digit())
        && message == "java.lang.RuntimeException: Failed to create the GLFW window"
}

pub fn classify_launch_failure(
    stdout_classes: &[FailureClass],
    exit_code: Option<i32>,
) -> Option<FailureClass> {
    if exit_code == Some(0) {
        return stdout_classes
            .contains(&FailureClass::GraphicsUnavailable)
            .then_some(FailureClass::GraphicsUnavailable);
    }
    Some(
        stdout_classes
            .iter()
            .copied()
            .filter(|class| *class != FailureClass::StartupStalled)
            .reduce(stronger_failure_class)
            .unwrap_or(FailureClass::Unknown),
    )
}

fn stronger_failure_class(left: FailureClass, right: FailureClass) -> FailureClass {
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
        FailureClass::GraphicsUnavailable => 8,
        FailureClass::MissingDependency => 9,
        FailureClass::ModTransformationFailure => 10,
        FailureClass::ClasspathModuleConflict => 11,
        FailureClass::LoaderBootstrapFailure => 12,
        FailureClass::AuthModeIncompatible => 13,
        FailureClass::Unknown => 14,
    }
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
            // Fabric's resolver failure, also inside a log4j XML throwable.
            || line.contains(
                "net.fabricmc.loader.impl.formattedexception: some of your mods are incompatible",
            )
            || (line.starts_with("- mod '")
                && ((line.contains(" requires ")
                    && (line.ends_with(", which is missing!")
                        || line.contains(", but only the wrong version is present: ")))
                    || (line.contains(" is incompatible with ")
                        && line.contains(", yet a conflicting version is present: "))))
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
        let evidence = LaunchEvidence {
            boot_observed: true,
            ..Default::default()
        };
        for facts in [
            SessionExitFacts {
                process_exited: false,
                ..exit_facts()
            },
            SessionExitFacts {
                tree_settled: false,
                ..exit_facts()
            },
            SessionExitFacts {
                outputs_drained: false,
                ..exit_facts()
            },
        ] {
            assert!(classify_session_outcome(facts, &evidence).is_none());
        }
        assert_eq!(
            classify_session_outcome(exit_facts(), &evidence)
                .unwrap()
                .kind,
            SessionOutcomeKind::Clean
        );
    }

    #[test]
    fn zero_exit_before_boot_is_unknown_and_stop_is_not_a_crash() {
        assert_eq!(
            classify_session_outcome(exit_facts(), &LaunchEvidence::default())
                .unwrap()
                .reason,
            SessionExitReason::UnknownExit
        );
        let facts = SessionExitFacts {
            exit_code: None,
            signal: Some(9),
            stop_requested: true,
            ..exit_facts()
        };
        assert_eq!(
            classify_session_outcome(facts, &LaunchEvidence::default())
                .unwrap()
                .reason,
            SessionExitReason::LauncherStopped
        );
        let facts = SessionExitFacts {
            stop_requested: false,
            ..facts
        };
        assert_eq!(
            classify_session_outcome(facts, &LaunchEvidence::default())
                .unwrap()
                .reason,
            SessionExitReason::CrashedBeforeBoot
        );
    }

    #[test]
    fn stopping_a_start_held_on_a_loader_dependency_error_is_a_failure() {
        let mut evidence = LaunchEvidence::default();
        for line in [
            "<log4j:Message><![CDATA[Incompatible mods found!]]></log4j:Message>",
            "<log4j:Throwable><![CDATA[net.fabricmc.loader.impl.FormattedException: Some of your mods are incompatible with the game or each other!",
            "\t - Mod 'Nuit Interop' (nuit_interop) 1.0.0 requires version 1.0.0 of nuit, which is missing!",
        ] {
            evidence.observe_line(line);
        }
        let facts = SessionExitFacts {
            exit_code: None,
            signal: Some(15),
            stop_requested: true,
            was_running: true,
            ..exit_facts()
        };
        let outcome = classify_session_outcome(facts, &evidence).unwrap();
        assert_eq!(
            (outcome.kind, outcome.reason, outcome.failure_class),
            (
                SessionOutcomeKind::Failed,
                SessionExitReason::StartupFailed,
                Some(FailureClass::MissingDependency)
            )
        );
        assert!(
            outcome
                .summary
                .starts_with("A mod needs another mod or loader version")
        );
        let booted = LaunchEvidence {
            boot_observed: true,
            ..evidence
        };
        assert_eq!(
            classify_session_outcome(facts, &booted).unwrap().reason,
            SessionExitReason::LauncherStopped
        );
    }

    #[test]
    fn explicit_stall_remains_the_reason_when_the_process_is_stopped() {
        let evidence = LaunchEvidence {
            boot_observed: false,
            failure_classes: vec![FailureClass::StartupStalled],
            ..Default::default()
        };
        let facts = SessionExitFacts {
            stop_requested: true,
            exit_code: Some(1),
            ..exit_facts()
        };
        assert_eq!(
            classify_session_outcome(facts, &evidence).unwrap().reason,
            SessionExitReason::StartupStalled
        );
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
                "FATAL ERROR in native method: [LWJGL] Thread[#3,Render thread,5,main]: No context is current or a function that is not available in the current context was called. The JVM will abort execution.",
                FailureClass::GraphicsUnavailable,
            ),
            (
                "FATAL ERROR in native method: [LWJGL] Thread[#41,Worker-Main-2,5,main]: No context is current or a function that is not available in the current context was called. The JVM will abort execution.",
                FailureClass::Unknown,
            ),
            (
                "[LWJGL] No context is current or a function that is not available in the current context was called.",
                FailureClass::Unknown,
            ),
            (
                "at com.mojang.renderpearl.backend.opengl.GlBackend.createDevice(GlBackend.java:68)",
                FailureClass::Unknown,
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
    fn glfw_api_diagnostics_require_an_uncaught_window_failure_in_either_stream_order() {
        const API: &str = "[LWJGL] GLFW_API_UNAVAILABLE error";
        const FATAL: &str = "<log4j:Message><![CDATA[[java.lang.ThreadGroup:uncaughtException:1052]: java.lang.RuntimeException: Failed to create the GLFW window]]></log4j:Message>";
        for lines in [[API, FATAL], [FATAL, API]] {
            let mut evidence = LaunchEvidence::default();
            evidence.observe_failure(FailureClass::LoaderBootstrapFailure);
            for line in lines {
                evidence.observe_line(line);
            }
            let outcome = classify_session_outcome(
                SessionExitFacts {
                    exit_code: Some(1),
                    ..exit_facts()
                },
                &evidence,
            )
            .unwrap();
            assert_eq!(
                outcome.failure_class,
                Some(FailureClass::GraphicsUnavailable)
            );
            assert_eq!(
                outcome.summary,
                "The game could not create a window because OpenGL is unavailable."
            );
        }
        for lines in [
            [
                API,
                "<log4j:Message><![CDATA[Failed to create window: ]]></log4j:Message>",
            ],
            [
                FATAL,
                "GLFW_API_UNAVAILABLE error troubleshooting documentation",
            ],
            [
                API,
                "<log4j:Message><![CDATA[[java.lang.ThreadGroup:uncaughtException:unknown]: java.lang.RuntimeException: Failed to create the GLFW window]]></log4j:Message>",
            ],
            [
                API,
                "<log4j:Message><![CDATA[java.lang.RuntimeException: Failed to create the GLFW window]]></log4j:Message>",
            ],
        ] {
            let mut evidence = LaunchEvidence::default();
            for line in lines {
                evidence.observe_line(line);
            }
            assert!(
                !evidence
                    .failure_classes
                    .contains(&FailureClass::GraphicsUnavailable)
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
            "\t - Mod 'Sodium Extra' (sodium-extra) 0.9.5 requires version 0.20.1 or later of mod 'Fabric Loader' (fabricloader), but only the wrong version is present: 0.19.3!",
            "net.fabricmc.loader.impl.FormattedException: Some of your mods are incompatible with the game or each other!",
            "<log4j:Throwable><![CDATA[net.fabricmc.loader.impl.FormattedException: Some of your mods are incompatible with the game or each other!",
            "- Mod 'Sodium' (sodium) 0.8.13 is incompatible with any version before 0.8.7 of mod 'Sodium Extra' (sodium-extra), yet a conflicting version is present: 0.6.0!",
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
    fn fusion_is_independent_of_candidate_order_and_ignores_lifecycle_state() {
        let mut candidates = vec![
            FailureClass::Unknown,
            FailureClass::OutOfMemory,
            FailureClass::JvmUnsupportedOption,
            FailureClass::MissingDependency,
            FailureClass::StartupStalled,
        ];
        assert_eq!(
            classify_launch_failure(&candidates, Some(1)),
            Some(FailureClass::JvmUnsupportedOption)
        );
        candidates.reverse();
        assert_eq!(
            classify_launch_failure(&candidates, Some(1)),
            Some(FailureClass::JvmUnsupportedOption)
        );
        assert_eq!(
            classify_launch_failure(&[FailureClass::StartupStalled], Some(1)),
            Some(FailureClass::Unknown)
        );
    }

    #[test]
    fn exit_status_table_distinguishes_clean_and_failed_processes_exactly() {
        let semantic = [FailureClass::JvmUnsupportedOption];
        for (exit_code, classes, expected) in [
            (Some(0), &[][..], None),
            (Some(0), &semantic[..], None),
            (Some(1), &[][..], Some(FailureClass::Unknown)),
            (None, &[][..], Some(FailureClass::Unknown)),
            (
                Some(-1),
                &semantic[..],
                Some(FailureClass::JvmUnsupportedOption),
            ),
        ] {
            assert_eq!(
                classify_launch_failure(classes, exit_code),
                expected,
                "exit code {exit_code:?}"
            );
        }
    }
}
