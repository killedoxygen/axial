use crate::crash::{
    CrashArtifactKind, CrashEvidence, CrashNativeFrameKind, is_out_of_memory_failure_line,
};
use crate::types::LaunchFailureClass;

pub fn classify_startup_failure_text(text: &str) -> LaunchFailureClass {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return LaunchFailureClass::Unknown;
    }
    if lower.contains("unrecognized vm option") || lower.contains("unsupported vm option") {
        return LaunchFailureClass::JvmUnsupportedOption;
    }
    if lower.contains("must be enabled via -xx:+unlockexperimentalvmoptions") {
        return LaunchFailureClass::JvmExperimentalUnlock;
    }
    if lower.contains("unlock option must precede")
        || lower.contains("unlockexperimentalvmoptions must precede")
        || lower.contains("unlockdiagnosticvmoptions must precede")
    {
        return LaunchFailureClass::JvmOptionOrdering;
    }
    if lower.contains("unsupportedclassversionerror")
        || lower.contains("compiled by a more recent version of the java runtime")
        || contains_requires_java_version(&lower)
    {
        return LaunchFailureClass::JavaRuntimeMismatch;
    }
    if contains_out_of_memory_failure(&lower) {
        return LaunchFailureClass::OutOfMemory;
    }
    if contains_artifact_signature_failure(&lower) {
        return LaunchFailureClass::LauncherManagedArtifactSignature;
    }
    if contains_missing_dependency_failure(&lower) {
        return LaunchFailureClass::MissingDependency;
    }
    if contains_mod_transformation_failure(&lower) {
        return LaunchFailureClass::ModTransformationFailure;
    }
    if lower.contains("resolutionexception: modules")
        || lower.contains("export package")
        || lower.contains("modulelayerhandler.buildlayer")
        || lower.contains("noclassdeffounderror")
        || lower.contains("classnotfoundexception")
        || lower.contains("failed to locate library:")
        || lower.contains("unsatisfiedlinkerror")
    {
        return LaunchFailureClass::ClasspathModuleConflict;
    }
    if lower.contains("nosuchelementexception: no value present")
        || (contains_loader_bootstrap_marker(&lower) && contains_failure_context(&lower))
    {
        return LaunchFailureClass::LoaderBootstrapFailure;
    }
    if lower.contains("microsoft account")
        || lower.contains("check your microsoft account")
        || lower.contains("multiplayer is disabled")
    {
        return LaunchFailureClass::AuthModeIncompatible;
    }
    LaunchFailureClass::Unknown
}

pub fn classify_launch_failure(
    stdout_classes: &[LaunchFailureClass],
    exit_code: Option<i32>,
    crash_evidence: Option<&CrashEvidence>,
) -> Option<LaunchFailureClass> {
    if exit_code == Some(0) {
        return None;
    }

    let stdout_class = stdout_classes
        .iter()
        .copied()
        .filter(|class| *class != LaunchFailureClass::StartupStalled)
        .reduce(stronger_failure_class);
    let evidence_class = crash_evidence.and_then(classify_crash_evidence);

    Some(match (stdout_class, evidence_class) {
        (Some(stdout), Some(evidence)) => stronger_failure_class(stdout, evidence),
        (Some(class), None) | (None, Some(class)) => class,
        (None, None) => LaunchFailureClass::Unknown,
    })
}

fn classify_crash_evidence(evidence: &CrashEvidence) -> Option<LaunchFailureClass> {
    if evidence.names_out_of_memory {
        return Some(LaunchFailureClass::OutOfMemory);
    }
    if evidence.source == CrashArtifactKind::JvmFatalError
        && evidence.problematic_frame.as_ref().is_some_and(|frame| {
            frame.kind == CrashNativeFrameKind::Native
                && is_graphics_driver_module(frame.module.as_str())
        })
    {
        return Some(LaunchFailureClass::GraphicsDriverCrash);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && evidence
            .exception_class
            .as_ref()
            .is_some_and(|class| is_missing_dependency_exception(class.as_str()))
    {
        return Some(LaunchFailureClass::MissingDependency);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && evidence
            .exception_class
            .as_ref()
            .is_some_and(|class| is_mod_transformation_exception(class.as_str()))
    {
        return Some(LaunchFailureClass::ModTransformationFailure);
    }
    if evidence.source == CrashArtifactKind::MinecraftCrashReport
        && !evidence.truncated
        && !evidence.suspected_mods.is_empty()
    {
        return Some(LaunchFailureClass::ModAttributedCrash);
    }
    None
}

fn stronger_failure_class(
    left: LaunchFailureClass,
    right: LaunchFailureClass,
) -> LaunchFailureClass {
    if failure_class_precedence(left) <= failure_class_precedence(right) {
        left
    } else {
        right
    }
}

fn failure_class_precedence(class: LaunchFailureClass) -> u8 {
    match class {
        LaunchFailureClass::StartupStalled => 0,
        LaunchFailureClass::JvmUnsupportedOption => 1,
        LaunchFailureClass::JvmExperimentalUnlock => 2,
        LaunchFailureClass::JvmOptionOrdering => 3,
        LaunchFailureClass::JavaRuntimeMismatch => 4,
        LaunchFailureClass::OutOfMemory => 5,
        LaunchFailureClass::LauncherManagedArtifactSignature => 6,
        LaunchFailureClass::GraphicsDriverCrash => 7,
        LaunchFailureClass::MissingDependency => 8,
        LaunchFailureClass::ModTransformationFailure => 9,
        LaunchFailureClass::ModAttributedCrash => 10,
        LaunchFailureClass::ClasspathModuleConflict => 11,
        LaunchFailureClass::LoaderBootstrapFailure => 12,
        LaunchFailureClass::AuthModeIncompatible => 13,
        LaunchFailureClass::Unknown => 14,
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
    use crate::crash::{MAX_CRASH_ARTIFACT_BYTES, parse_crash_evidence};

    const RANKED_FAILURES: [LaunchFailureClass; 15] = [
        LaunchFailureClass::StartupStalled,
        LaunchFailureClass::JvmUnsupportedOption,
        LaunchFailureClass::JvmExperimentalUnlock,
        LaunchFailureClass::JvmOptionOrdering,
        LaunchFailureClass::JavaRuntimeMismatch,
        LaunchFailureClass::OutOfMemory,
        LaunchFailureClass::LauncherManagedArtifactSignature,
        LaunchFailureClass::GraphicsDriverCrash,
        LaunchFailureClass::MissingDependency,
        LaunchFailureClass::ModTransformationFailure,
        LaunchFailureClass::ModAttributedCrash,
        LaunchFailureClass::ClasspathModuleConflict,
        LaunchFailureClass::LoaderBootstrapFailure,
        LaunchFailureClass::AuthModeIncompatible,
        LaunchFailureClass::Unknown,
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
                LaunchFailureClass::JvmUnsupportedOption,
            ),
            (
                "java.lang.UnsupportedClassVersionError: compiled by a more recent version of the Java Runtime",
                LaunchFailureClass::JavaRuntimeMismatch,
            ),
            (
                "Mod Example requires Java 17 or later",
                LaunchFailureClass::JavaRuntimeMismatch,
            ),
            (
                "UnlockExperimentalVMOptions must precede 'UseZGC'",
                LaunchFailureClass::JvmOptionOrdering,
            ),
            (
                "Caused by: java.lang.NoClassDefFoundError: org/lwjgl/glfw/GLFW",
                LaunchFailureClass::ClasspathModuleConflict,
            ),
            (
                "java.lang.UnsatisfiedLinkError: Failed to locate library: lwjgl.dll",
                LaunchFailureClass::ClasspathModuleConflict,
            ),
            (
                "java.lang.SecurityException: class net.minecraft.SomeClass signer information does not match signer information of other classes in the same package",
                LaunchFailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "Exception in thread \"main\" java.lang.SecurityException: Invalid signature file digest for Manifest main attributes",
                LaunchFailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "java.lang.SecurityException: SHA-256 digest error for net/minecraft/client/Minecraft.class",
                LaunchFailureClass::LauncherManagedArtifactSignature,
            ),
            (
                "[main/INFO] [cpw.mods.modlauncher.Launcher/MODLAUNCHER]: ModLauncher running",
                LaunchFailureClass::Unknown,
            ),
            (
                "[EARLYDISPLAY/]: If this message is the only thing at the bottom of your log before a crash, you probably have a driver issue.",
                LaunchFailureClass::Unknown,
            ),
            (
                "This troubleshooting guide requires Java knowledge",
                LaunchFailureClass::Unknown,
            ),
            (
                "The Example mod must precede another mod in the load order",
                LaunchFailureClass::Unknown,
            ),
            (
                "cpw.mods.modlauncher.api.IncompatibleEnvironmentException: failed to load transformation service",
                LaunchFailureClass::LoaderBootstrapFailure,
            ),
            ("ordinary launcher output", LaunchFailureClass::Unknown),
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
                LaunchFailureClass::OutOfMemory,
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
                LaunchFailureClass::Unknown,
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
                LaunchFailureClass::MissingDependency,
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
                LaunchFailureClass::ModTransformationFailure,
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
                LaunchFailureClass::Unknown,
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
        candidates.push(LaunchFailureClass::StartupStalled);
        candidates.push(LaunchFailureClass::JvmUnsupportedOption);
        assert_eq!(
            classify_launch_failure(&candidates, Some(1), None),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        candidates.reverse();
        assert_eq!(
            classify_launch_failure(&candidates, Some(1), None),
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        assert_eq!(
            classify_launch_failure(&[LaunchFailureClass::StartupStalled], Some(1), None),
            Some(LaunchFailureClass::Unknown)
        );
    }

    #[test]
    fn exit_status_table_distinguishes_clean_and_failed_processes_exactly() {
        let oom =
            report("Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space");
        let semantic = [LaunchFailureClass::JvmUnsupportedOption];
        for (exit_code, classes, evidence, expected) in [
            (Some(0), &[][..], None, None),
            (Some(0), &semantic[..], Some(&oom), None),
            (Some(1), &[][..], None, Some(LaunchFailureClass::Unknown)),
            (None, &[][..], None, Some(LaunchFailureClass::Unknown)),
            (
                Some(-1),
                &semantic[..],
                None,
                Some(LaunchFailureClass::JvmUnsupportedOption),
            ),
            (
                None,
                &[][..],
                Some(&oom),
                Some(LaunchFailureClass::OutOfMemory),
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
            LaunchFailureClass::AuthModeIncompatible,
            LaunchFailureClass::ClasspathModuleConflict,
        ];
        let cases = [
            (
                report(
                    "Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space\nSuspected Mods: Example Mod (example)",
                ),
                LaunchFailureClass::OutOfMemory,
            ),
            (
                report(
                    "Description: Loading game\nnet.minecraftforge.fml.common.MissingModsException: missing\nSuspected Mods: Example Mod (example)",
                ),
                LaunchFailureClass::MissingDependency,
            ),
            (
                report(
                    "Description: Loading game\norg.spongepowered.asm.mixin.transformer.throwables.MixinApplyError: failed\nSuspected Mods: Example Mod (example)",
                ),
                LaunchFailureClass::ModTransformationFailure,
            ),
            (
                report(
                    "Description: Rendering game\njava.lang.IllegalStateException: failed\nSuspected Mods: Example Mod (example)",
                ),
                LaunchFailureClass::ModAttributedCrash,
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
            Some(LaunchFailureClass::GraphicsDriverCrash)
        );
        assert_eq!(
            classify_launch_failure(
                &[LaunchFailureClass::LauncherManagedArtifactSignature],
                Some(1),
                Some(&graphics),
            ),
            Some(LaunchFailureClass::LauncherManagedArtifactSignature)
        );

        let oom =
            report("Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space");
        assert_eq!(
            classify_launch_failure(
                &[LaunchFailureClass::JavaRuntimeMismatch],
                Some(1),
                Some(&oom),
            ),
            Some(LaunchFailureClass::JavaRuntimeMismatch)
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
            Some(LaunchFailureClass::Unknown)
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
            Some(LaunchFailureClass::Unknown)
        );

        let fatal = hs_err('C', "libjvm");
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&fatal)),
            Some(LaunchFailureClass::Unknown)
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
            Some(LaunchFailureClass::Unknown)
        );

        let mut graphics = hs_err('C', "nvoglv64");
        graphics.source = CrashArtifactKind::MinecraftCrashReport;
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&graphics)),
            Some(LaunchFailureClass::Unknown)
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
            Some(LaunchFailureClass::MissingDependency)
        );

        let mut raw =
            b"Description: Rendering game\njava.lang.OutOfMemoryError: Java heap space\n".to_vec();
        raw.resize(MAX_CRASH_ARTIFACT_BYTES + 1, b'x');
        let oom = parse_crash_evidence(CrashArtifactKind::MinecraftCrashReport, &raw)
            .expect("truncated out-of-memory evidence");
        assert!(oom.truncated);
        assert_eq!(
            classify_launch_failure(&[], Some(1), Some(&oom)),
            Some(LaunchFailureClass::OutOfMemory)
        );
    }
}
