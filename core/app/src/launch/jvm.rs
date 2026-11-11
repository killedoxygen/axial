use axial_minecraft::JavaRuntimeInfo;

pub const PRESET_SMOOTH: &str = "smooth";
pub const PRESET_PERFORMANCE: &str = "performance";
pub const PRESET_ULTRA_LOW_LATENCY: &str = "ultra_low_latency";
pub const PRESET_GRAALVM: &str = "graalvm";
pub const PRESET_LEGACY: &str = "legacy";
pub const PRESET_LEGACY_PVP: &str = "legacy_pvp";
pub const PRESET_LEGACY_HEAVY: &str = "legacy_heavy";

const ULTRA_LOW_LATENCY_MIN_LOGICAL_CORES: usize = 8;
const ULTRA_LOW_LATENCY_MIN_TOTAL_MEMORY_MB: u64 = 8 * 1024;

pub(crate) fn gc_preset_args(
    preset: &str,
    info: &JavaRuntimeInfo,
    low_impact_startup: bool,
) -> Vec<String> {
    match preset.trim() {
        PRESET_SMOOTH => smooth_args(low_impact_startup),
        PRESET_ULTRA_LOW_LATENCY => ultra_low_latency_args(info, low_impact_startup),
        PRESET_GRAALVM => graalvm_args(low_impact_startup),
        PRESET_LEGACY => conservative_g1_args(200, low_impact_startup),
        PRESET_LEGACY_PVP => conservative_g1_args(15, low_impact_startup),
        PRESET_LEGACY_HEAVY => conservative_g1_args(100, low_impact_startup),
        PRESET_PERFORMANCE => conservative_g1_args(37, low_impact_startup),
        _ => Vec::new(),
    }
}

fn smooth_args(low_impact_startup: bool) -> Vec<String> {
    let mut args = vec![
        "-XX:+UseShenandoahGC".to_string(),
        "-XX:ShenandoahGCHeuristics=compact".to_string(),
        "-XX:+DisableExplicitGC".to_string(),
        "-XX:+PerfDisableSharedMem".to_string(),
    ];
    if !low_impact_startup {
        args.push("-XX:+AlwaysPreTouch".to_string());
    }
    args
}

pub(crate) fn boot_throttle_args(java_major: u32, cpus: usize) -> Vec<String> {
    let budget = cpus.saturating_sub(2).max(2);
    let ci_threads = (budget / 2).clamp(2, 4);

    let mut args = vec![format!("-XX:CICompilerCount={ci_threads}")];
    if java_major >= 9 {
        let gc_threads = budget.min(6);
        args.push(format!("-XX:ParallelGCThreads={gc_threads}"));
        args.push(format!("-XX:ConcGCThreads={ci_threads}"));
    }
    args
}

pub(crate) fn sanitize_preset(
    preset: &str,
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> String {
    // This is the compatibility gate for user and managed JVM preset choices.
    if !supports_hotspot_tuning(info) {
        return String::new();
    }

    let legacy_family = is_legacy_family(version_id);
    let preset = preset.trim();

    if info.major <= 8 {
        if legacy_family
            && matches!(
                preset,
                PRESET_LEGACY | PRESET_LEGACY_PVP | PRESET_LEGACY_HEAVY
            )
        {
            return preset.to_string();
        }
        return legacy_preset_for_target(version_id, loader, is_modded).to_string();
    }

    match preset {
        PRESET_LEGACY | PRESET_LEGACY_PVP | PRESET_LEGACY_HEAVY if legacy_family => {
            return PRESET_LEGACY.to_string();
        }
        PRESET_LEGACY | PRESET_LEGACY_PVP | PRESET_LEGACY_HEAVY => {
            return PRESET_PERFORMANCE.to_string();
        }
        PRESET_SMOOTH if !supports_shenandoah(info) || legacy_family => {
            return PRESET_PERFORMANCE.to_string();
        }
        PRESET_ULTRA_LOW_LATENCY if !supports_zgc(info) => {
            return PRESET_PERFORMANCE.to_string();
        }
        PRESET_GRAALVM if info.distribution != "graalvm" || info.major < 17 => {
            return PRESET_PERFORMANCE.to_string();
        }
        _ => {}
    }

    if legacy_family && matches!(preset, PRESET_ULTRA_LOW_LATENCY | PRESET_SMOOTH) {
        return PRESET_PERFORMANCE.to_string();
    }
    if is_modded_launch(loader, is_modded) && preset == PRESET_ULTRA_LOW_LATENCY {
        return PRESET_PERFORMANCE.to_string();
    }

    preset.to_string()
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

pub(super) fn auto_select_preset_with_host(
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
    logical_cores: Option<usize>,
    total_memory_mb: Option<u64>,
) -> String {
    if !supports_hotspot_tuning(info) {
        return String::new();
    }
    if info.distribution == "graalvm" && info.major >= 17 {
        return PRESET_GRAALVM.to_string();
    }
    if info.major <= 8 {
        return legacy_preset_for_target(version_id, loader, is_modded).to_string();
    }
    if is_legacy_family(version_id) {
        return PRESET_PERFORMANCE.to_string();
    }
    if is_modded_launch(loader, is_modded) {
        return PRESET_PERFORMANCE.to_string();
    }
    if supports_generational_zgc(info) && is_ultra_low_latency_host(logical_cores, total_memory_mb)
    {
        return PRESET_ULTRA_LOW_LATENCY.to_string();
    }
    if supports_shenandoah(info) {
        return PRESET_SMOOTH.to_string();
    }
    PRESET_PERFORMANCE.to_string()
}

fn is_ultra_low_latency_host(logical_cores: Option<usize>, total_memory_mb: Option<u64>) -> bool {
    logical_cores.is_some_and(|value| value >= ULTRA_LOW_LATENCY_MIN_LOGICAL_CORES)
        && total_memory_mb.is_some_and(|value| value >= ULTRA_LOW_LATENCY_MIN_TOTAL_MEMORY_MB)
}

fn ultra_low_latency_args(info: &JavaRuntimeInfo, low_impact_startup: bool) -> Vec<String> {
    let mut args = vec![
        "-XX:+UseZGC".to_string(),
        "-XX:+DisableExplicitGC".to_string(),
        "-XX:+PerfDisableSharedMem".to_string(),
    ];
    if !low_impact_startup {
        args.push("-XX:+AlwaysPreTouch".to_string());
    }
    // JEP 490 makes ZGC generational unconditionally in JDK 24 and obsoletes
    // this selector. Newer runtimes need only UseZGC.
    if supports_generational_zgc(info) && info.major < 24 {
        args.push("-XX:+ZGenerational".to_string());
    }
    args
}

fn graalvm_args(low_impact_startup: bool) -> Vec<String> {
    let mut args = vec![
        "-XX:+UseG1GC".to_string(),
        "-XX:+EnableJVMCI".to_string(),
        "-XX:+UseJVMCICompiler".to_string(),
        "-XX:-TieredCompilation".to_string(),
        "-XX:ReservedCodeCacheSize=256M".to_string(),
        "-XX:InitialCodeCacheSize=256M".to_string(),
        "-XX:+DisableExplicitGC".to_string(),
        "-XX:MaxInlineLevel=15".to_string(),
        "-XX:MaxInlineSize=270".to_string(),
    ];
    if !low_impact_startup {
        args.push("-XX:+AlwaysPreTouch".to_string());
    }
    args
}

fn conservative_g1_args(pause_millis: i32, low_impact_startup: bool) -> Vec<String> {
    let mut args = vec![
        "-XX:+UseG1GC".to_string(),
        format!("-XX:MaxGCPauseMillis={pause_millis}"),
        "-XX:+ParallelRefProcEnabled".to_string(),
        "-XX:+UnlockExperimentalVMOptions".to_string(),
        "-XX:+DisableExplicitGC".to_string(),
        "-XX:G1NewSizePercent=20".to_string(),
        "-XX:G1MaxNewSizePercent=60".to_string(),
        "-XX:G1HeapRegionSize=16M".to_string(),
        "-XX:G1ReservePercent=15".to_string(),
        "-XX:G1HeapWastePercent=5".to_string(),
        "-XX:G1MixedGCCountTarget=4".to_string(),
        "-XX:InitiatingHeapOccupancyPercent=20".to_string(),
        "-XX:G1MixedGCLiveThresholdPercent=90".to_string(),
        "-XX:G1RSetUpdatingPauseTimePercent=5".to_string(),
        "-XX:SurvivorRatio=32".to_string(),
        "-XX:+PerfDisableSharedMem".to_string(),
    ];
    if !low_impact_startup {
        args.push("-XX:+AlwaysPreTouch".to_string());
    }
    args
}

fn is_legacy_family(version_id: &str) -> bool {
    let parts = version_id
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return false;
    }
    parts[0] == 1 && parts[1] <= 12
}

fn legacy_preset_for_target(version_id: &str, loader: &str, is_modded: bool) -> &'static str {
    if version_id == "1.8.9" {
        return PRESET_LEGACY_PVP;
    }
    if version_id == "1.12.2" && is_modded_launch(loader, is_modded) {
        return PRESET_LEGACY_HEAVY;
    }
    PRESET_LEGACY
}

fn is_modded_launch(loader: &str, is_modded: bool) -> bool {
    loader == "forge" || loader == "neoforge" || is_modded
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
    fn automatic_preset_follows_target_runtime_and_host() {
        let host = (Some(16), Some(32_768));
        for (version, loader, modded, major, distribution, (cores, memory), expected) in [
            ("1.20.4", "vanilla", false, 11, "openjdk", (Some(4), Some(4096)), PRESET_SMOOTH),
            ("1.20.4", "vanilla", false, 17, "openjdk", (Some(8), Some(8191)), PRESET_SMOOTH),
            ("1.20.6", "vanilla", false, 21, "openjdk", (Some(7), Some(8192)), PRESET_SMOOTH),
            ("1.20.6", "vanilla", false, 21, "openjdk", (Some(8), Some(8191)), PRESET_SMOOTH),
            ("1.21.1", "vanilla", false, 21, "openjdk", (Some(8), Some(8192)), PRESET_ULTRA_LOW_LATENCY),
            ("1.21.1", "vanilla", false, 24, "openjdk", (Some(12), Some(16_384)), PRESET_ULTRA_LOW_LATENCY),
            ("1.21.1", "fabric", true, 21, "openjdk", host, PRESET_PERFORMANCE),
            ("1.20.4", "vanilla", false, 17, "graalvm", (Some(4), Some(4096)), PRESET_GRAALVM),
            ("1.20.4", "forge", true, 21, "graalvm", (Some(8), Some(8192)), PRESET_GRAALVM),
            ("1.20.4", "vanilla", false, 21, "openj9", host, ""),
            ("1.8.9", "vanilla", false, 8, "openjdk", (Some(4), Some(4096)), PRESET_LEGACY_PVP),
            ("1.12.2", "forge", true, 8, "openjdk", (Some(4), Some(4096)), PRESET_LEGACY_HEAVY),
            ("1.7.10", "vanilla", false, 8, "openjdk", (Some(4), Some(4096)), PRESET_LEGACY),
            ("1.12.2", "vanilla", false, 8, "openjdk", (Some(4), Some(4096)), PRESET_LEGACY),
            ("1.8.9", "vanilla", false, 17, "openjdk", host, PRESET_PERFORMANCE),
            ("1.12.2", "forge", true, 21, "openjdk", host, PRESET_PERFORMANCE),
        ] {
            let info = info(major, distribution);
            let selected =
                auto_select_preset_with_host(version, loader, modded, &info, cores, memory);
            assert_eq!(selected, expected, "{version} {loader} java {major} {distribution}");
            assert_eq!(
                sanitize_preset(&selected, version, loader, modded, &info),
                selected,
                "automatic selection must already satisfy compatibility"
            );
        }
    }

    #[test]
    fn explicit_presets_are_sanitized_for_the_target_and_runtime() {
        for (preset, version, loader, modded, major, distribution, expected) in [
            (PRESET_PERFORMANCE, "1.20.4", "vanilla", false, 21, "openj9", ""),
            (PRESET_GRAALVM, "1.20.4", "forge", true, 21, "graalvm", PRESET_GRAALVM),
            (PRESET_PERFORMANCE, "1.8.9", "vanilla", false, 8, "openjdk", PRESET_LEGACY_PVP),
            (PRESET_LEGACY_PVP, "1.8.9", "vanilla", false, 8, "openjdk", PRESET_LEGACY_PVP),
            (PRESET_LEGACY_HEAVY, "1.12.2", "forge", true, 8, "openjdk", PRESET_LEGACY_HEAVY),
            (PRESET_LEGACY, "1.20.4", "vanilla", false, 21, "openjdk", PRESET_PERFORMANCE),
            (PRESET_LEGACY_HEAVY, "1.20.4", "vanilla", false, 21, "openjdk", PRESET_PERFORMANCE),
            (PRESET_SMOOTH, "1.8.9", "vanilla", false, 8, "openjdk", PRESET_LEGACY_PVP),
            (PRESET_ULTRA_LOW_LATENCY, "1.12.2", "forge", true, 8, "openjdk", PRESET_LEGACY_HEAVY),
        ] {
            assert_eq!(
                sanitize_preset(preset, version, loader, modded, &info(major, distribution)),
                expected,
                "{preset} on {version} java {major} {distribution}"
            );
        }
    }

    #[test]
    fn runtime_specific_flags_follow_java_version_and_management() {
        let modern = info(21, "openjdk");
        assert!(ultra_low_latency_args(&modern, false).contains(&"-XX:+ZGenerational".into()));
        let args = ultra_low_latency_args(&info(24, "openjdk"), false);
        assert!(args.contains(&"-XX:+UseZGC".into()));
        assert!(!args.contains(&"-XX:+ZGenerational".into()));
        for managed in [true, false] {
            let args = gc_preset_args(PRESET_PERFORMANCE, &modern, managed);
            assert_eq!(args.iter().any(|arg| arg == "-XX:+AlwaysPreTouch"), !managed);
        }
    }
}
