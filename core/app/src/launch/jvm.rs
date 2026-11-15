//! The one launch JVM policy: the launcher flags each performance mode adds to
//! a game's own version arguments, explicit preset compatibility, and heap
//! resolution. Explicit choices are honoured or refused, never rewritten.
//!
//! - Managed with the Automatic preset adds only what the runtime's own
//!   ergonomics lack for Minecraft: G1 on Java 8, whose default is Parallel GC.
//! - Vanilla adds nothing beyond the heap.
//! - Custom adds only the user's explicit preset.
//!
//! No mode caps compiler or GC threads for the process lifetime or pre-touches
//! the heap: both slowed measured startup and neither is reversible after boot.

use crate::settings::ConfigPerformanceMode;
use axial_minecraft::JavaRuntimeInfo;

pub const PRESET_SMOOTH: &str = "smooth";
pub const PRESET_PERFORMANCE: &str = "performance";
pub const PRESET_ULTRA_LOW_LATENCY: &str = "ultra_low_latency";
pub const PRESET_GRAALVM: &str = "graalvm";
pub const PRESET_LEGACY: &str = "legacy";
pub const PRESET_LEGACY_PVP: &str = "legacy_pvp";
pub const PRESET_LEGACY_HEAVY: &str = "legacy_heavy";

/// The heap an Automatic choice aims for when the host has room.
const AUTOMATIC_HEAP_TARGET_MB: u64 = 4096;
/// The smallest Automatic heap; a host with less still gets half its memory.
const AUTOMATIC_HEAP_FLOOR_MB: u64 = 1024;
/// Memory the operating system, the launcher and the game's off-heap use
/// (metaspace, code cache, direct buffers, native libraries) keep from a heap.
const AUTOMATIC_HEADROOM_MB: u64 = 1536;
const HEAP_GRANULE_MB: u64 = 256;

/// The launcher flags for `mode` and an already admitted explicit `preset`
/// (empty for Automatic).
pub(crate) fn launcher_args(
    mode: ConfigPerformanceMode,
    preset: &str,
    info: &JavaRuntimeInfo,
) -> Vec<String> {
    match (mode, preset.trim()) {
        (ConfigPerformanceMode::Vanilla, _) => Vec::new(),
        (ConfigPerformanceMode::Managed, "") => managed_defaults(info),
        (ConfigPerformanceMode::Custom, "") => Vec::new(),
        (_, preset) => preset_args(preset, info),
    }
}

fn managed_defaults(info: &JavaRuntimeInfo) -> Vec<String> {
    if info.major <= 8 && supports_hotspot_tuning(info) {
        vec!["-XX:+UseG1GC".into()]
    } else {
        Vec::new()
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
            if supports_generational_zgc(info) && info.major < 24 {
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

/// Whether an explicit preset runs on this target and runtime. An explicit
/// incompatible choice is refused, never replaced by another preset.
pub(crate) fn preset_supported(preset: &str, version_id: &str, info: &JavaRuntimeInfo) -> bool {
    let legacy_family = is_legacy_family(version_id);
    match preset.trim() {
        "" => true,
        _ if !supports_hotspot_tuning(info) => false,
        PRESET_LEGACY | PRESET_LEGACY_PVP | PRESET_LEGACY_HEAVY => legacy_family,
        PRESET_SMOOTH => supports_shenandoah(info) && !legacy_family,
        PRESET_ULTRA_LOW_LATENCY => supports_zgc(info) && !legacy_family,
        PRESET_GRAALVM => info.distribution == "graalvm" && info.major >= 17,
        PRESET_PERFORMANCE => true,
        _ => false,
    }
}

pub fn supports_hotspot_tuning(info: &JavaRuntimeInfo) -> bool {
    info.distribution != "openj9"
}

pub fn supports_shenandoah(info: &JavaRuntimeInfo) -> bool {
    supports_hotspot_tuning(info) && info.major >= 11 && info.distribution != "graalvm"
}

pub fn supports_zgc(info: &JavaRuntimeInfo) -> bool {
    supports_hotspot_tuning(info) && info.major >= 17 && info.distribution != "graalvm"
}

pub fn supports_generational_zgc(info: &JavaRuntimeInfo) -> bool {
    supports_zgc(info) && info.major >= 21
}

fn is_legacy_family(version_id: &str) -> bool {
    let mut parts = version_id.split('.').map(str::parse::<u32>);
    matches!((parts.next(), parts.next()), (Some(Ok(1)), Some(Ok(minor))) if minor <= 12)
}

/// The Automatic maximum heap from a completed host observation: the target
/// when the host has room, never more than half of its memory or its free
/// memory after headroom, and never below the floor a host can afford.
/// Available memory already excludes running games' resident memory, so their
/// committed heaps are not subtracted again. A missing sample (zero) is
/// unknown, not empty: it does not constrain the choice.
pub(crate) fn automatic_max_heap_mb(total_mb: u64, available_mb: u64) -> i32 {
    let mut heap = AUTOMATIC_HEAP_TARGET_MB;
    if total_mb > 0 {
        let half = total_mb / 2;
        heap = heap.min(half);
        if available_mb > 0 {
            heap = heap.min(available_mb.saturating_sub(AUTOMATIC_HEADROOM_MB));
        }
        heap = heap.max(AUTOMATIC_HEAP_FLOOR_MB.min(half));
    }
    let heap = (heap / HEAP_GRANULE_MB * HEAP_GRANULE_MB).max(crate::settings::HEAP_MIN_MB as u64);
    i32::try_from(heap).unwrap_or(crate::settings::HEAP_MIN_MB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(major: u32, distribution: &str) -> JavaRuntimeInfo {
        JavaRuntimeInfo {
            id: "runtime".to_string(),
            major,
            update: 0,
            distribution: distribution.to_string(),
            path: "/java".to_string(),
        }
    }

    #[test]
    fn modes_add_only_their_own_flags() {
        let modern = info(21, "openjdk");
        let legacy = info(8, "openjdk");
        assert!(launcher_args(ConfigPerformanceMode::Managed, "", &modern).is_empty());
        assert_eq!(
            launcher_args(ConfigPerformanceMode::Managed, "", &legacy),
            ["-XX:+UseG1GC"]
        );
        assert!(launcher_args(ConfigPerformanceMode::Managed, "", &info(8, "openj9")).is_empty());
        for preset in ["", PRESET_PERFORMANCE] {
            assert!(launcher_args(ConfigPerformanceMode::Vanilla, preset, &legacy).is_empty());
        }
        assert!(launcher_args(ConfigPerformanceMode::Custom, "", &legacy).is_empty());
        let custom = launcher_args(ConfigPerformanceMode::Custom, PRESET_PERFORMANCE, &modern);
        assert!(custom.contains(&"-XX:MaxGCPauseMillis=37".into()));
        for mode in [
            ConfigPerformanceMode::Managed,
            ConfigPerformanceMode::Custom,
        ] {
            for preset in [
                "",
                PRESET_SMOOTH,
                PRESET_PERFORMANCE,
                PRESET_ULTRA_LOW_LATENCY,
                PRESET_LEGACY,
            ] {
                let args = launcher_args(mode, preset, &modern);
                assert!(
                    !args.iter().any(|arg| arg.starts_with("-XX:CICompilerCount")
                        || arg.starts_with("-XX:ParallelGCThreads")
                        || arg.starts_with("-XX:ConcGCThreads")
                        || arg == "-XX:+AlwaysPreTouch"),
                    "{preset}: {args:?}"
                );
            }
        }
    }

    #[test]
    fn explicit_presets_are_admitted_only_where_they_run() {
        for (preset, version, major, distribution, supported) in [
            (PRESET_PERFORMANCE, "1.20.4", 21, "openj9", false),
            (PRESET_GRAALVM, "1.20.4", 21, "graalvm", true),
            (PRESET_GRAALVM, "1.20.4", 21, "openjdk", false),
            (PRESET_LEGACY_PVP, "1.8.9", 8, "openjdk", true),
            (PRESET_LEGACY, "1.20.4", 21, "openjdk", false),
            (PRESET_SMOOTH, "1.8.9", 17, "openjdk", false),
            (PRESET_SMOOTH, "1.20.4", 8, "openjdk", false),
            (PRESET_ULTRA_LOW_LATENCY, "1.21.1", 21, "openjdk", true),
            (PRESET_ULTRA_LOW_LATENCY, "1.21.1", 11, "openjdk", false),
            ("", "1.21.1", 21, "openj9", true),
        ] {
            assert_eq!(
                preset_supported(preset, version, &info(major, distribution)),
                supported,
                "{preset} on {version} java {major} {distribution}"
            );
        }
        assert!(
            preset_args(PRESET_ULTRA_LOW_LATENCY, &info(21, "openjdk"))
                .contains(&"-XX:+ZGenerational".into())
        );
        assert!(
            !preset_args(PRESET_ULTRA_LOW_LATENCY, &info(24, "openjdk"))
                .contains(&"-XX:+ZGenerational".into())
        );
    }

    #[test]
    fn automatic_heap_fits_the_observed_host() {
        for (total, available, expected) in [
            (24_576, 11_000, 4096),
            (24_576, 4_000, 2304),
            (24_576, 1_000, 1024),
            (6_144, 3_800, 2048),
            (4_096, 3_000, 1280),
            (1_536, 1_200, 768),
            (0, 0, 4096),
            (16_384, 0, 4096),
        ] {
            assert_eq!(
                automatic_max_heap_mb(total, available),
                expected,
                "{total} {available}"
            );
        }
    }
}
