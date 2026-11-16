//! The one launch JVM policy: the launcher flags each performance mode adds to
//! a game's own version arguments and explicit preset compatibility. Explicit
//! choices are honoured or refused, never rewritten.
//!
//! - Managed with the Automatic preset adds only what the runtime's own
//!   ergonomics lack for Minecraft: G1 on Java 8, whose default is Parallel GC,
//!   and only where that exact VM proves it applies G1.
//! - Vanilla adds nothing beyond the heap.
//! - Custom adds only the user's explicit preset.
//!
//! A preset's flags are admitted only after the exact verified runtime starts
//! with them and reports each applied (`JavaProbeReceipt::applies_options`);
//! vendor names and major versions are never taken as capability.
//!
//! No mode caps compiler or GC threads for the process lifetime or pre-touches
//! the heap: both slowed measured startup and neither is reversible after boot.

use super::model::LaunchPlanError;
use crate::settings::ConfigPerformanceMode;
use axial_minecraft::{JavaProbeReceipt, JavaRuntimeInfo};

pub const PRESET_SMOOTH: &str = "smooth";
pub const PRESET_PERFORMANCE: &str = "performance";
pub const PRESET_ULTRA_LOW_LATENCY: &str = "ultra_low_latency";
pub const PRESET_GRAALVM: &str = "graalvm";
pub const PRESET_LEGACY: &str = "legacy";
pub const PRESET_LEGACY_PVP: &str = "legacy_pvp";
pub const PRESET_LEGACY_HEAVY: &str = "legacy_heavy";


/// The launcher flags `mode` and `preset` add on this exact runtime. An
/// explicit preset is refused unless the VM applies every one of its flags;
/// Automatic adds Java 8's G1 only where the VM applies it, and nothing else.
pub(crate) async fn proven_launcher_args(
    mode: ConfigPerformanceMode,
    preset: &str,
    probe: &JavaProbeReceipt,
    cancellation: impl std::future::Future<Output = ()>,
) -> Result<Vec<String>, LaunchPlanError> {
    let (args, explicit) = match (mode, preset.trim()) {
        (ConfigPerformanceMode::Vanilla, _) | (ConfigPerformanceMode::Custom, "") => {
            return Ok(Vec::new());
        }
        (ConfigPerformanceMode::Managed, "") if probe.info().major <= 8 => {
            (vec!["-XX:+UseG1GC".to_string()], false)
        }
        (ConfigPerformanceMode::Managed, "") => return Ok(Vec::new()),
        (_, preset) => (preset_args(preset, probe.info()), true),
    };
    if args.is_empty() {
        return Err(LaunchPlanError::IncompatiblePreset);
    }
    match probe.applies_options(&args, cancellation).await {
        Ok(true) => Ok(args),
        Ok(false) if explicit => Err(LaunchPlanError::IncompatiblePreset),
        Ok(false) => Ok(Vec::new()),
        // Callers observe their own cancellation before classifying this.
        Err(_) => Err(LaunchPlanError::RuntimeChanged),
    }
}

fn preset_args(preset: &str, info: &JavaRuntimeInfo) -> Vec<String> {
    let mut args = match preset {
        PRESET_SMOOTH => vec![
            "-XX:+UseShenandoahGC".to_string(),
            "-XX:ShenandoahGCHeuristics=compact".to_string(),
        ],
        PRESET_ULTRA_LOW_LATENCY => {
            let mut args = vec!["-XX:+UseZGC".to_string()];
            // JEP 490 makes ZGC generational unconditionally in JDK 24 and
            // obsoletes this selector.
            if (21..24).contains(&info.major) {
                args.push("-XX:+ZGenerational".to_string());
            }
            args
        }
        PRESET_GRAALVM => vec![
            "-XX:+UseG1GC".to_string(),
            "-XX:+EnableJVMCI".to_string(),
            "-XX:+UseJVMCICompiler".to_string(),
        ],
        PRESET_LEGACY => g1_args(200),
        PRESET_LEGACY_PVP => g1_args(15),
        PRESET_LEGACY_HEAVY => g1_args(100),
        PRESET_PERFORMANCE => g1_args(37),
        _ => return Vec::new(),
    };
    args.push("-XX:+DisableExplicitGC".to_string());
    args
}

fn g1_args(pause_millis: i32) -> Vec<String> {
    vec![
        "-XX:+UseG1GC".to_string(),
        format!("-XX:MaxGCPauseMillis={pause_millis}"),
        "-XX:+UnlockExperimentalVMOptions".to_string(),
        "-XX:G1NewSizePercent=20".to_string(),
        "-XX:G1ReservePercent=20".to_string(),
        "-XX:G1HeapRegionSize=16M".to_string(),
    ]
}

/// Whether an explicit preset is meant for this game target: the legacy
/// presets for alpha, beta and releases through 1.12, the concurrent collectors
/// for newer versions.
/// Whether the runtime can run it is proven separately on the exact VM.
pub(crate) fn preset_targets(preset: &str, version_id: &str, version_kind: &str) -> bool {
    let legacy_family = is_legacy_family(version_id, version_kind);
    match preset.trim() {
        "" | PRESET_PERFORMANCE | PRESET_GRAALVM => true,
        PRESET_LEGACY | PRESET_LEGACY_PVP | PRESET_LEGACY_HEAVY => legacy_family,
        PRESET_SMOOTH | PRESET_ULTRA_LOW_LATENCY => !legacy_family,
        _ => false,
    }
}

fn is_legacy_family(version_id: &str, version_kind: &str) -> bool {
    if matches!(version_kind, "old_alpha" | "old_beta") {
        return true;
    }
    let mut parts = version_id.split('.').map(str::parse::<u32>);
    matches!((parts.next(), parts.next()), (Some(Ok(1)), Some(Ok(minor))) if minor <= 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_presets_target_their_own_game_family() {
        for (preset, version, targeted) in [
            (PRESET_LEGACY_PVP, "1.8.9", true),
            (PRESET_LEGACY, "1.20.4", false),
            (PRESET_SMOOTH, "1.8.9", false),
            (PRESET_ULTRA_LOW_LATENCY, "1.21.1", true),
            (PRESET_PERFORMANCE, "1.8.9", true),
            (PRESET_GRAALVM, "1.20.4", true),
            ("", "1.21.1", true),
            ("unknown", "1.21.1", false),
        ] {
            assert_eq!(
                preset_targets(preset, version, "release"),
                targeted,
                "{preset} on {version}"
            );
        }
        for preset in [
            PRESET_SMOOTH,
            PRESET_PERFORMANCE,
            PRESET_ULTRA_LOW_LATENCY,
            PRESET_LEGACY,
        ] {
            let args = preset_args(preset, &info(21));
            assert!(
                !args.iter().any(|arg| arg.starts_with("-XX:CICompilerCount")
                    || arg.starts_with("-XX:ParallelGCThreads")
                    || arg.starts_with("-XX:ConcGCThreads")
                    || arg == "-XX:+AlwaysPreTouch"),
                "{preset}: {args:?}"
            );
        }
        assert!(
            preset_args(PRESET_ULTRA_LOW_LATENCY, &info(21)).contains(&"-XX:+ZGenerational".into())
        );
        assert!(
            !preset_args(PRESET_ULTRA_LOW_LATENCY, &info(24))
                .contains(&"-XX:+ZGenerational".into())
        );
    }

    fn info(major: u32) -> JavaRuntimeInfo {
        JavaRuntimeInfo {
            id: String::new(),
            major,
            update: 0,
            distribution: String::new(),
            path: String::new(),
        }
    }

    /// A VM that answers flag-table probes with `axial-minecraft`'s
    /// `runtime/flag_tables`: captured HotSpot 25 and Temurin 8 tables,
    /// and an OpenJ9 response written to its no-table layout.
    #[cfg(unix)]
    async fn vm(root: &std::path::Path, version: &str, g1_table: &str) -> JavaProbeReceipt {
        use std::os::unix::fs::PermissionsExt as _;
        let tables = "../minecraft/src/runtime/flag_tables";
        let table = |name: &str| {
            std::fs::canonicalize(format!("{}/{tables}/{name}", env!("CARGO_MANIFEST_DIR")))
                .unwrap()
                .display()
                .to_string()
        };
        let java = root.join("java");
        std::fs::write(
            &java,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n\
                 *EnableJVMCI*) cat '{refused}' >&2; exit 1;;\n\
                 *UseZGC*) cat '{zgc}';;\n\
                 *PrintFlagsFinal*) cat '{g1}';;\n\
                 *) printf 'java.version = {version}\\nos.arch = aarch64\\n' >&2;;\n\
                 esac\n",
                refused = table("hotspot25-jvmci-refused.txt"),
                zgc = table("hotspot25-zgc.txt"),
                g1 = table(g1_table),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        axial_minecraft::probe_java(&java, None, std::future::pending())
            .await
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launcher_flags_are_admitted_only_where_the_exact_vm_applies_them() {
        use ConfigPerformanceMode::{Custom, Managed, Vanilla};
        let roots: Vec<_> = (0..3)
            .map(|_| {
                tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
            })
            .collect();
        let modern = vm(roots[0].path(), "25.0.1", "hotspot25-g1.txt").await;
        let legacy = vm(roots[1].path(), "1.8.0_512", "temurin8-g1.txt").await;
        let openj9 = vm(roots[2].path(), "1.8.0_472", "openj9-no-table.txt").await;
        let proven =
            |mode, preset, probe| proven_launcher_args(mode, preset, probe, std::future::pending());

        assert_eq!(proven(Managed, "", &modern).await, Ok(Vec::new()));
        assert_eq!(
            proven(Managed, "", &legacy).await,
            Ok(vec!["-XX:+UseG1GC".to_string()])
        );
        let legacy_args = proven(Managed, PRESET_LEGACY, &legacy).await.unwrap();
        assert!(legacy_args.contains(&"-XX:MaxGCPauseMillis=200".to_string()));
        assert!(legacy_args.contains(&"-XX:G1ReservePercent=20".to_string()));
        // Automatic adds nothing where the VM does not prove G1, and refuses nothing.
        assert_eq!(proven(Managed, "", &openj9).await, Ok(Vec::new()));
        assert_eq!(
            proven(Vanilla, PRESET_PERFORMANCE, &openj9).await,
            Ok(Vec::new())
        );
        assert_eq!(proven(Custom, "", &openj9).await, Ok(Vec::new()));

        let performance = proven(Custom, PRESET_PERFORMANCE, &modern).await.unwrap();
        assert!(performance.contains(&"-XX:MaxGCPauseMillis=37".to_string()));
        assert_eq!(
            proven(Managed, PRESET_ULTRA_LOW_LATENCY, &modern).await,
            Ok(vec![
                "-XX:+UseZGC".to_string(),
                "-XX:+DisableExplicitGC".to_string()
            ])
        );
        // Explicit presets the VM refuses or silently ignores are refused, never replaced.
        for (preset, probe) in [
            (PRESET_GRAALVM, &modern),
            (PRESET_PERFORMANCE, &openj9),
            (PRESET_LEGACY_PVP, &openj9),
        ] {
            assert_eq!(
                proven(Custom, preset, probe).await,
                Err(LaunchPlanError::IncompatiblePreset),
                "{preset}"
            );
        }
        assert_eq!(
            proven(Custom, "unknown", &modern).await,
            Err(LaunchPlanError::IncompatiblePreset)
        );
    }
}
