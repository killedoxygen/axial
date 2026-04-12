use croopor_minecraft::JavaRuntimeInfo;

pub const PRESET_SMOOTH: &str = "smooth";
pub const PRESET_PERFORMANCE: &str = "performance";
pub const PRESET_ULTRA_LOW_LATENCY: &str = "ultra_low_latency";
pub const PRESET_GRAALVM: &str = "graalvm";
pub const PRESET_LEGACY: &str = "legacy";
pub const PRESET_LEGACY_PVP: &str = "legacy_pvp";
pub const PRESET_LEGACY_HEAVY: &str = "legacy_heavy";

pub fn resolve_preset(
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

pub fn gc_preset_args(
    preset: &str,
    info: &JavaRuntimeInfo,
    low_impact_startup: bool,
) -> Vec<String> {
    let preset = sanitize_preset(preset, "", "vanilla", false, info);
    match preset.as_str() {
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

pub fn boot_throttle_args(java_major: u32) -> Vec<String> {
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

pub fn sanitize_preset(
    preset: &str,
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> String {
    if !supports_hotspot_tuning(info) {
        return String::new();
    }

    let legacy_family = is_legacy_family(version_id);
    let preset = preset.trim();

    if info.major <= 8 {
        return PRESET_LEGACY.to_string();
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
    if (loader == "forge" || loader == "neoforge" || is_modded)
        && preset == PRESET_ULTRA_LOW_LATENCY
    {
        return PRESET_PERFORMANCE.to_string();
    }

    preset.to_string()
}

pub fn supports_hotspot_tuning(info: &JavaRuntimeInfo) -> bool {
    info.distribution != "openj9"
}

pub fn supports_shenandoah(info: &JavaRuntimeInfo) -> bool {
    supports_hotspot_tuning(info) && info.major >= 17 && info.distribution != "graalvm"
}

pub fn supports_zgc(info: &JavaRuntimeInfo) -> bool {
    supports_hotspot_tuning(info) && info.major >= 17 && info.distribution != "graalvm"
}

pub fn supports_generational_zgc(info: &JavaRuntimeInfo) -> bool {
    supports_zgc(info) && (21..=23).contains(&info.major)
}

fn auto_select_preset(
    version_id: &str,
    loader: &str,
    is_modded: bool,
    info: &JavaRuntimeInfo,
) -> String {
    if !supports_hotspot_tuning(info) {
        return String::new();
    }
    if info.distribution == "graalvm" && info.major >= 17 && !is_modded {
        return PRESET_GRAALVM.to_string();
    }
    if info.major <= 8 {
        return PRESET_LEGACY.to_string();
    }
    if is_legacy_family(version_id) {
        return PRESET_PERFORMANCE.to_string();
    }
    if loader == "forge" || loader == "neoforge" || is_modded {
        return PRESET_PERFORMANCE.to_string();
    }
    if supports_shenandoah(info) {
        return PRESET_SMOOTH.to_string();
    }
    PRESET_PERFORMANCE.to_string()
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
    let version = base_version_id(version_id);
    let parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return false;
    }
    parts[0] == 1 && parts[1] <= 12
}

fn base_version_id(version_id: &str) -> String {
    if let Some((base, _)) = version_id.split_once("-forge-") {
        return base.to_string();
    }
    if let Some(value) = version_id.strip_prefix("neoforge-") {
        return value.to_string();
    }
    if let Some(captures) = regex::Regex::new(r"^(?:fabric|quilt)-loader-.+-(\d+\.\d+(?:\.\d+)?)$")
        .ok()
        .and_then(|regex| regex.captures(version_id))
        && let Some(matched) = captures.get(1)
    {
        return matched.as_str().to_string();
    }
    version_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(major: u32) -> JavaRuntimeInfo {
        JavaRuntimeInfo {
            id: "runtime".to_string(),
            major,
            update: 0,
            distribution: "openjdk".to_string(),
            path: "/java".to_string(),
        }
    }

    #[test]
    fn legacy_runtime_forces_legacy_preset() {
        assert_eq!(
            sanitize_preset(PRESET_PERFORMANCE, "1.8.9", "vanilla", false, &info(8)),
            PRESET_LEGACY
        );
    }

    #[test]
    fn legacy_preset_downgrades_to_performance_on_modern_runtime() {
        assert_eq!(
            sanitize_preset(PRESET_LEGACY, "1.20.4", "vanilla", false, &info(21)),
            PRESET_PERFORMANCE
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
