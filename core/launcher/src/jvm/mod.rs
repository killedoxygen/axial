use axial_minecraft::JavaRuntimeInfo;
use sysinfo::System;

pub const PRESET_SMOOTH: &str = "smooth";
pub const PRESET_PERFORMANCE: &str = "performance";
pub const PRESET_ULTRA_LOW_LATENCY: &str = "ultra_low_latency";
pub const PRESET_GRAALVM: &str = "graalvm";
pub const PRESET_LEGACY: &str = "legacy";
pub const PRESET_LEGACY_PVP: &str = "legacy_pvp";
pub const PRESET_LEGACY_HEAVY: &str = "legacy_heavy";

const ULTRA_LOW_LATENCY_MIN_LOGICAL_CORES: usize = 8;
const ULTRA_LOW_LATENCY_MIN_TOTAL_MEMORY_MB: u64 = 8 * 1024;

pub(crate) fn recommended_preset(
    requested: &str,
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> String {
    let requested = requested.trim();
    let preset = if requested.is_empty() {
        auto_select_preset(version_id, loader, is_modded, info)
    } else {
        requested.to_string()
    };
    sanitize_preset(&preset, version_id, loader, is_modded, info)
}

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

pub(crate) fn boot_throttle_args(java_major: u32) -> Vec<String> {
    let cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
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

fn auto_select_preset(
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> String {
    let (logical_cores, total_memory_mb) = host_evidence();
    auto_select_preset_with_host(
        version_id,
        loader,
        is_modded,
        info,
        logical_cores,
        total_memory_mb,
    )
}

fn auto_select_preset_with_host(
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

fn host_evidence() -> (Option<usize>, Option<u64>) {
    let logical_cores = std::thread::available_parallelism().ok().map(usize::from);

    let mut system = System::new();
    system.refresh_memory();
    let total_memory_mb = system.total_memory() / (1024 * 1024);

    (
        logical_cores,
        (total_memory_mb > 0).then_some(total_memory_mb),
    )
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
    if supports_generational_zgc(info) {
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

    fn info(major: u32) -> JavaRuntimeInfo {
        info_with_distribution(major, "openjdk")
    }

    fn info_with_distribution(major: u32, distribution: &str) -> JavaRuntimeInfo {
        JavaRuntimeInfo {
            id: "runtime".to_string(),
            major,
            update: 0,
            distribution: distribution.to_string(),
            path: "/java".to_string(),
        }
    }

    #[test]
    fn phase1_auto_select_preset_gate_matrix() {
        struct Case {
            name: &'static str,
            version_id: &'static str,
            loader: &'static str,
            is_modded: bool,
            info: JavaRuntimeInfo,
            logical_cores: Option<usize>,
            total_memory_mb: Option<u64>,
            expected: &'static str,
        }

        let cases = [
            Case {
                name: "java 11 modern vanilla smooth on lower-spec host",
                version_id: "1.20.4",
                loader: "vanilla",
                is_modded: false,
                info: info(11),
                logical_cores: Some(4),
                total_memory_mb: Some(4096),
                expected: PRESET_SMOOTH,
            },
            Case {
                name: "java 17 modern vanilla smooth on non-ultra host",
                version_id: "1.20.4",
                loader: "vanilla",
                is_modded: false,
                info: info(17),
                logical_cores: Some(8),
                total_memory_mb: Some(8191),
                expected: PRESET_SMOOTH,
            },
            Case {
                name: "java 21 high-end modern vanilla ultra low latency",
                version_id: "1.21.1",
                loader: "vanilla",
                is_modded: false,
                info: info(21),
                logical_cores: Some(8),
                total_memory_mb: Some(8192),
                expected: PRESET_ULTRA_LOW_LATENCY,
            },
            Case {
                name: "high-end modern modded performance",
                version_id: "1.21.1",
                loader: "fabric",
                is_modded: true,
                info: info(21),
                logical_cores: Some(16),
                total_memory_mb: Some(32_768),
                expected: PRESET_PERFORMANCE,
            },
            Case {
                name: "graalvm java 17 vanilla preserves graalvm tuning",
                version_id: "1.20.4",
                loader: "vanilla",
                is_modded: false,
                info: info_with_distribution(17, "graalvm"),
                logical_cores: Some(4),
                total_memory_mb: Some(4096),
                expected: PRESET_GRAALVM,
            },
            Case {
                name: "graalvm java 21 modded preserves graalvm tuning",
                version_id: "1.20.4",
                loader: "forge",
                is_modded: true,
                info: info_with_distribution(21, "graalvm"),
                logical_cores: Some(16),
                total_memory_mb: Some(32_768),
                expected: PRESET_GRAALVM,
            },
            Case {
                name: "normalized openj9 semeru ibm runtime disables hotspot tuning",
                version_id: "1.20.4",
                loader: "vanilla",
                is_modded: false,
                info: info_with_distribution(21, "openj9"),
                logical_cores: Some(16),
                total_memory_mb: Some(32_768),
                expected: "",
            },
            Case {
                name: "java 8 1.8.9 uses legacy pvp",
                version_id: "1.8.9",
                loader: "vanilla",
                is_modded: false,
                info: info(8),
                logical_cores: Some(4),
                total_memory_mb: Some(4096),
                expected: PRESET_LEGACY_PVP,
            },
            Case {
                name: "java 8 modded 1.12.2 uses legacy heavy",
                version_id: "1.12.2",
                loader: "forge",
                is_modded: true,
                info: info(8),
                logical_cores: Some(4),
                total_memory_mb: Some(4096),
                expected: PRESET_LEGACY_HEAVY,
            },
            Case {
                name: "java 8 other legacy target uses conservative legacy",
                version_id: "1.7.10",
                loader: "vanilla",
                is_modded: false,
                info: info(8),
                logical_cores: Some(4),
                total_memory_mb: Some(4096),
                expected: PRESET_LEGACY,
            },
            Case {
                name: "modern java targeting legacy vanilla uses safe hotspot preset",
                version_id: "1.8.9",
                loader: "vanilla",
                is_modded: false,
                info: info(17),
                logical_cores: Some(16),
                total_memory_mb: Some(32_768),
                expected: PRESET_PERFORMANCE,
            },
            Case {
                name: "modern java targeting legacy modded uses safe hotspot preset",
                version_id: "1.12.2",
                loader: "forge",
                is_modded: true,
                info: info(21),
                logical_cores: Some(16),
                total_memory_mb: Some(32_768),
                expected: PRESET_PERFORMANCE,
            },
        ];

        for case in cases {
            assert_eq!(
                auto_select_preset_with_host(
                    case.version_id,
                    case.loader,
                    case.is_modded,
                    &case.info,
                    case.logical_cores,
                    case.total_memory_mb,
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn openj9_runtime_skips_hotspot_tuning() {
        let info = info_with_distribution(21, "openj9");

        assert_eq!(
            recommended_preset("", "1.20.4", "vanilla", false, &info),
            ""
        );
        assert_eq!(
            sanitize_preset(PRESET_PERFORMANCE, "1.20.4", "vanilla", false, &info),
            ""
        );
    }

    #[test]
    fn modern_non_graalvm_defaults_stay_on_hotspot_presets() {
        assert_eq!(
            auto_select_preset_with_host(
                "1.20.4",
                "vanilla",
                false,
                &info(21),
                Some(4),
                Some(8192)
            ),
            PRESET_SMOOTH
        );
        assert_eq!(
            auto_select_preset_with_host(
                "1.20.4",
                "forge",
                true,
                &info(21),
                Some(16),
                Some(32_768)
            ),
            PRESET_PERFORMANCE
        );
    }

    #[test]
    fn high_end_modern_vanilla_auto_selects_ultra_low_latency() {
        assert_eq!(
            auto_select_preset_with_host(
                "1.20.6",
                "vanilla",
                false,
                &info(21),
                Some(8),
                Some(8192)
            ),
            PRESET_ULTRA_LOW_LATENCY
        );
        assert_eq!(
            auto_select_preset_with_host(
                "1.21.1",
                "vanilla",
                false,
                &info(24),
                Some(12),
                Some(16_384)
            ),
            PRESET_ULTRA_LOW_LATENCY
        );
    }

    #[test]
    fn modern_vanilla_below_high_end_threshold_keeps_smooth() {
        assert_eq!(
            auto_select_preset_with_host(
                "1.20.6",
                "vanilla",
                false,
                &info(21),
                Some(7),
                Some(8192)
            ),
            PRESET_SMOOTH
        );
        assert_eq!(
            auto_select_preset_with_host(
                "1.20.6",
                "vanilla",
                false,
                &info(21),
                Some(8),
                Some(8191)
            ),
            PRESET_SMOOTH
        );
    }

    #[test]
    fn graalvm_runtime_auto_selects_graalvm_for_modded_launch() {
        let info = info_with_distribution(21, "graalvm");

        assert_eq!(
            recommended_preset("", "1.20.4", "forge", true, &info),
            PRESET_GRAALVM
        );
    }

    #[test]
    fn explicit_graalvm_preset_survives_modded_graalvm_runtime() {
        let info = info_with_distribution(21, "graalvm");

        assert_eq!(
            sanitize_preset(PRESET_GRAALVM, "1.20.4", "forge", true, &info),
            PRESET_GRAALVM
        );
    }

    #[test]
    fn java8_legacy_targets_auto_select_specific_legacy_presets() {
        assert_eq!(
            recommended_preset("", "1.8.9", "vanilla", false, &info(8)),
            PRESET_LEGACY_PVP
        );
        assert_eq!(
            recommended_preset("", "1.12.2", "forge", true, &info(8)),
            PRESET_LEGACY_HEAVY
        );
        assert_eq!(
            recommended_preset("", "1.12.2", "vanilla", false, &info(8)),
            PRESET_LEGACY
        );
    }

    #[test]
    fn legacy_runtime_uses_target_specific_safe_preset() {
        assert_eq!(
            sanitize_preset(PRESET_PERFORMANCE, "1.8.9", "vanilla", false, &info(8)),
            PRESET_LEGACY_PVP
        );
    }

    #[test]
    fn java8_legacy_targets_preserve_legacy_variants() {
        assert_eq!(
            sanitize_preset(PRESET_LEGACY_PVP, "1.8.9", "vanilla", false, &info(8)),
            PRESET_LEGACY_PVP
        );
        assert_eq!(
            sanitize_preset(PRESET_LEGACY_HEAVY, "1.12.2", "forge", true, &info(8)),
            PRESET_LEGACY_HEAVY
        );
    }

    #[test]
    fn legacy_preset_downgrades_to_performance_on_modern_runtime() {
        assert_eq!(
            sanitize_preset(PRESET_LEGACY, "1.20.4", "vanilla", false, &info(21)),
            PRESET_PERFORMANCE
        );
        assert_eq!(
            sanitize_preset(PRESET_LEGACY_HEAVY, "1.20.4", "vanilla", false, &info(21)),
            PRESET_PERFORMANCE
        );
    }

    #[test]
    fn unsafe_modern_presets_downgrade_on_java8_legacy_targets() {
        assert_eq!(
            sanitize_preset(PRESET_SMOOTH, "1.8.9", "vanilla", false, &info(8)),
            PRESET_LEGACY_PVP
        );
        assert_eq!(
            sanitize_preset(PRESET_ULTRA_LOW_LATENCY, "1.12.2", "forge", true, &info(8)),
            PRESET_LEGACY_HEAVY
        );
    }

    #[test]
    fn managed_modes_skip_always_pre_touch() {
        let args = gc_preset_args(PRESET_PERFORMANCE, &info(21), true);
        assert!(!args.iter().any(|arg| arg == "-XX:+AlwaysPreTouch"));
    }

    #[test]
    fn custom_modes_keep_always_pre_touch() {
        let args = gc_preset_args(PRESET_PERFORMANCE, &info(21), false);
        assert!(args.iter().any(|arg| arg == "-XX:+AlwaysPreTouch"));
    }
}
