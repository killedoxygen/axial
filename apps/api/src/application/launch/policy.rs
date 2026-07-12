use axial_config::{AppConfig, Instance};
use axial_launcher::{GuardianMode, LAUNCH_MEMORY_HEADROOM_MB, OverrideOrigin, SessionId};
use axial_minecraft::{VersionEntry, compare_version_like};
use std::cmp::Ordering;
use std::time::SystemTime;

const BUILT_IN_MAX_MEMORY_MB: i32 = 4096;
const BUILT_IN_MIN_MEMORY_MB: i32 = 512;
const MIN_DERIVED_MAX_MEMORY_MB: i32 = 1024;
const LEGACY_MAX_MEMORY_TARGET_MB: i32 = 2048;
const MODERN_VANILLA_MAX_MEMORY_TARGET_MB: i32 = 4096;
const MODDED_MAX_MEMORY_TARGET_MB: i32 = 6144;
const DERIVED_MIN_MEMORY_TARGET_MB: i32 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LaunchMemoryDefaults {
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
}

pub(super) fn selected_java_override(instance: &Instance, config: &AppConfig) -> String {
    if !instance.java_path.trim().is_empty() {
        instance.java_path.trim().to_string()
    } else {
        config.java_path_override.trim().to_string()
    }
}

pub(super) fn selected_jvm_preset(instance: &Instance, config: &AppConfig) -> String {
    if !instance.jvm_preset.trim().is_empty() {
        instance.jvm_preset.trim().to_string()
    } else {
        config.jvm_preset.trim().to_string()
    }
}

pub(super) fn selected_performance_mode(instance: &Instance, config: &AppConfig) -> String {
    if !instance.performance_mode.trim().is_empty() {
        instance.performance_mode.trim().to_string()
    } else {
        config.performance_mode.trim().to_string()
    }
}

pub(super) fn selected_resolution(instance: &Instance, config: &AppConfig) -> Option<(u32, u32)> {
    let width = if instance.window_width > 0 {
        instance.window_width
    } else {
        config.window_width
    };
    let height = if instance.window_height > 0 {
        instance.window_height
    } else {
        config.window_height
    };
    if width > 0 && height > 0 {
        Some((width as u32, height as u32))
    } else {
        None
    }
}

pub(super) fn effective_max_memory(
    instance: &Instance,
    config: &AppConfig,
    requested: Option<i32>,
    defaults: Option<LaunchMemoryDefaults>,
) -> i32 {
    if instance.max_memory_mb > 0 {
        instance.max_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else if let Some(defaults) = defaults {
        defaults.max_memory_mb
    } else {
        config.max_memory_mb
    }
}

pub(super) fn effective_min_memory(
    instance: &Instance,
    config: &AppConfig,
    requested: Option<i32>,
    max_memory_mb: i32,
    defaults: Option<LaunchMemoryDefaults>,
) -> i32 {
    selected_raw_min_memory(instance, config, requested, defaults)
        .min(max_memory_mb)
        .max(0)
}

pub(super) fn selected_raw_min_memory(
    instance: &Instance,
    config: &AppConfig,
    requested: Option<i32>,
    defaults: Option<LaunchMemoryDefaults>,
) -> i32 {
    if instance.min_memory_mb > 0 {
        instance.min_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else if let Some(defaults) = defaults {
        defaults.min_memory_mb
    } else {
        config.min_memory_mb
    }
}

pub(super) fn derived_launch_memory_defaults(
    instance: &Instance,
    config: &AppConfig,
    version: Option<&VersionEntry>,
    requested_max_memory_mb: Option<i32>,
    requested_min_memory_mb: Option<i32>,
    host_total_memory_mb: Option<u64>,
) -> Option<LaunchMemoryDefaults> {
    if instance.max_memory_mb > 0 || instance.min_memory_mb > 0 {
        return None;
    }
    if requested_max_memory_mb.unwrap_or_default() > 0
        || requested_min_memory_mb.unwrap_or_default() > 0
    {
        return None;
    }
    if config.max_memory_mb != BUILT_IN_MAX_MEMORY_MB
        || config.min_memory_mb != BUILT_IN_MIN_MEMORY_MB
    {
        return None;
    }

    launch_memory_defaults_for_host_version(
        host_total_memory_mb?,
        version_base_id(instance, version).as_str(),
        version.is_some_and(|version| version.loader.is_some()),
    )
}

fn launch_memory_defaults_for_host_version(
    host_total_memory_mb: u64,
    version_id: &str,
    is_modded: bool,
) -> Option<LaunchMemoryDefaults> {
    let host_budget_mb = host_total_memory_mb.saturating_sub(LAUNCH_MEMORY_HEADROOM_MB);
    if host_budget_mb == 0 {
        return None;
    }

    let target_max_memory_mb = if is_modded {
        MODDED_MAX_MEMORY_TARGET_MB
    } else if version_id_is_legacy(version_id) {
        LEGACY_MAX_MEMORY_TARGET_MB
    } else {
        MODERN_VANILLA_MAX_MEMORY_TARGET_MB
    };
    let host_limited_max_memory_mb = i32::try_from(host_budget_mb).unwrap_or(i32::MAX);
    let max_memory_mb = target_max_memory_mb
        .min(host_limited_max_memory_mb)
        .max(MIN_DERIVED_MAX_MEMORY_MB);
    let min_memory_mb = DERIVED_MIN_MEMORY_TARGET_MB.min(max_memory_mb).max(0);

    Some(LaunchMemoryDefaults {
        max_memory_mb,
        min_memory_mb,
    })
}

pub(super) fn suggested_max_memory_after_recent_oom(
    current_max_memory_mb: i32,
    host_total_memory_mb: Option<u64>,
    active_memory_allocation_mb: u64,
    version_id: &str,
    is_modded: bool,
) -> Option<i32> {
    if current_max_memory_mb <= 0 || version_id.trim().is_empty() {
        return None;
    }

    let host_after_active_memory_mb =
        host_total_memory_mb?.checked_sub(active_memory_allocation_mb)?;
    let safe_max_memory_mb = host_after_active_memory_mb.checked_sub(LAUNCH_MEMORY_HEADROOM_MB)?;
    if safe_max_memory_mb < u64::try_from(MIN_DERIVED_MAX_MEMORY_MB).ok()? {
        return None;
    }

    let target = launch_memory_defaults_for_host_version(
        host_after_active_memory_mb,
        version_id.trim(),
        is_modded,
    )?;
    let suggested_max_memory_mb = u64::try_from(target.max_memory_mb)
        .ok()?
        .min(safe_max_memory_mb);
    let suggested_max_memory_mb = i32::try_from(suggested_max_memory_mb).ok()?;
    (suggested_max_memory_mb > current_max_memory_mb).then_some(suggested_max_memory_mb)
}

fn version_base_id(instance: &Instance, version: Option<&VersionEntry>) -> String {
    version
        .and_then(|version| {
            let parent = version.inherits_from.trim();
            (!parent.is_empty()).then(|| parent.to_string())
        })
        .or_else(|| version.map(|version| version.id.clone()))
        .unwrap_or_else(|| instance.version_id.clone())
}

fn version_id_is_legacy(version_id: &str) -> bool {
    compare_version_like(version_id, "1.12.2") != Ordering::Greater
}

pub(super) fn selected_guardian_mode(config: &AppConfig) -> GuardianMode {
    GuardianMode::from_config(&config.guardian_mode)
}

pub(super) fn java_override_origin(
    instance: &Instance,
    config: &AppConfig,
) -> Option<OverrideOrigin> {
    if !instance.java_path.trim().is_empty() {
        Some(OverrideOrigin::Instance)
    } else if !config.java_path_override.trim().is_empty() {
        Some(OverrideOrigin::Global)
    } else {
        None
    }
}

pub(super) fn preset_override_origin(
    instance: &Instance,
    config: &AppConfig,
) -> Option<OverrideOrigin> {
    if !instance.jvm_preset.trim().is_empty() {
        Some(OverrideOrigin::Instance)
    } else if !config.jvm_preset.trim().is_empty() {
        Some(OverrideOrigin::Global)
    } else {
        None
    }
}

pub(super) fn raw_jvm_args_origin(instance: &Instance) -> Option<OverrideOrigin> {
    (!instance.extra_jvm_args.trim().is_empty()).then_some(OverrideOrigin::Instance)
}

pub(super) fn generate_session_id() -> SessionId {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    SessionId(format!("{:032x}", nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_minecraft::build_id_for;

    #[test]
    fn explicit_instance_memory_takes_precedence_over_request_and_derived_defaults() {
        let config = AppConfig::default();
        let mut instance = test_instance("1.21.1");
        instance.max_memory_mb = 3072;
        instance.min_memory_mb = 1536;

        let defaults = derived_launch_memory_defaults(
            &instance,
            &config,
            None,
            Some(8192),
            Some(4096),
            Some(16_384),
        );

        assert_eq!(defaults, None);
        assert_eq!(
            effective_max_memory(&instance, &config, Some(8192), defaults),
            3072
        );
        assert_eq!(
            selected_raw_min_memory(&instance, &config, Some(4096), defaults),
            1536
        );
        assert_eq!(
            effective_min_memory(&instance, &config, Some(4096), 3072, defaults),
            1536
        );
    }

    #[test]
    fn explicit_request_memory_takes_precedence_over_derived_defaults() {
        let config = AppConfig::default();
        let instance = test_instance("1.21.1");

        let defaults = derived_launch_memory_defaults(
            &instance,
            &config,
            None,
            Some(5120),
            Some(2048),
            Some(16_384),
        );

        assert_eq!(defaults, None);
        assert_eq!(
            effective_max_memory(&instance, &config, Some(5120), defaults),
            5120
        );
        assert_eq!(
            selected_raw_min_memory(&instance, &config, Some(2048), defaults),
            2048
        );
        assert_eq!(
            effective_min_memory(&instance, &config, Some(2048), 5120, defaults),
            2048
        );
    }

    #[test]
    fn custom_global_memory_blocks_derived_defaults_for_fresh_instances() {
        let config = AppConfig {
            max_memory_mb: 5120,
            min_memory_mb: 768,
            ..AppConfig::default()
        };
        let instance = test_instance("1.21.1");

        let defaults =
            derived_launch_memory_defaults(&instance, &config, None, None, None, Some(16_384));

        assert_eq!(defaults, None);
        assert_eq!(
            effective_max_memory(&instance, &config, None, defaults),
            5120
        );
        assert_eq!(
            selected_raw_min_memory(&instance, &config, None, defaults),
            768
        );
    }

    #[test]
    fn fresh_legacy_vanilla_uses_legacy_memory_defaults() {
        let config = AppConfig::default();
        let instance = test_instance("1.12.2");

        let defaults =
            derived_launch_memory_defaults(&instance, &config, None, None, None, Some(16_384))
                .expect("legacy defaults");

        assert_eq!(
            defaults,
            LaunchMemoryDefaults {
                max_memory_mb: 2048,
                min_memory_mb: 1024,
            }
        );
        assert_eq!(
            effective_max_memory(&instance, &config, None, Some(defaults)),
            2048
        );
        assert_eq!(
            effective_min_memory(&instance, &config, None, 2048, Some(defaults)),
            1024
        );
    }

    #[test]
    fn fresh_modern_vanilla_uses_modern_memory_defaults() {
        let config = AppConfig::default();
        let instance = test_instance("1.21.1");

        let defaults =
            derived_launch_memory_defaults(&instance, &config, None, None, None, Some(16_384))
                .expect("modern defaults");

        assert_eq!(
            defaults,
            LaunchMemoryDefaults {
                max_memory_mb: 4096,
                min_memory_mb: 1024,
            }
        );
    }

    #[test]
    fn fresh_loader_target_uses_modded_memory_defaults() {
        let config = AppConfig::default();
        let instance = test_instance("fabric-loader-0.16.10-1.21.1");
        let version = test_loader_version(&instance.version_id, "1.21.1");

        let defaults = derived_launch_memory_defaults(
            &instance,
            &config,
            Some(&version),
            None,
            None,
            Some(16_384),
        )
        .expect("modded defaults");

        assert_eq!(
            defaults,
            LaunchMemoryDefaults {
                max_memory_mb: 6144,
                min_memory_mb: 1024,
            }
        );
    }

    #[test]
    fn derived_defaults_leave_host_headroom_when_host_budget_is_smaller_than_target() {
        let config = AppConfig::default();
        let instance = test_instance("fabric-loader-0.16.10-1.21.1");
        let version = test_loader_version(&instance.version_id, "1.21.1");

        let defaults = derived_launch_memory_defaults(
            &instance,
            &config,
            Some(&version),
            None,
            None,
            Some(6_144),
        )
        .expect("host-limited defaults");

        assert_eq!(
            defaults,
            LaunchMemoryDefaults {
                max_memory_mb: 4096,
                min_memory_mb: 1024,
            }
        );
    }

    #[test]
    fn recent_oom_suggestion_uses_safe_low_end_and_modded_targets() {
        assert_eq!(
            suggested_max_memory_after_recent_oom(1024, Some(4096), 0, "1.21.1", false),
            Some(2048)
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(4096, Some(16_384), 0, "1.21.1", true),
            Some(6144)
        );
    }

    #[test]
    fn recent_oom_suggestion_retains_version_family_caps() {
        for (version_id, is_modded, expected) in [
            ("1.12.2", false, 2048),
            ("1.21.1", false, 4096),
            ("1.21.1", true, 6144),
        ] {
            assert_eq!(
                suggested_max_memory_after_recent_oom(1024, Some(16_384), 0, version_id, is_modded),
                Some(expected),
                "version={version_id}, modded={is_modded}",
            );
        }
    }

    #[test]
    fn recent_oom_suggestion_reserves_active_allocations_and_os_headroom() {
        assert_eq!(
            suggested_max_memory_after_recent_oom(1024, Some(8192), 3072, "1.21.1", true),
            Some(3072)
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(1024, Some(4096), 2048, "1.21.1", false),
            None
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(512, Some(2560), 0, "1.21.1", false),
            None
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(512, Some(3072), 0, "1.21.1", false),
            Some(1024)
        );
    }

    #[test]
    fn recent_oom_suggestion_requires_a_strict_increase() {
        assert_eq!(
            suggested_max_memory_after_recent_oom(4096, Some(6144), 0, "1.21.1", true),
            None
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(4096, Some(6145), 0, "1.21.1", true),
            Some(4097)
        );
        assert_eq!(
            suggested_max_memory_after_recent_oom(6144, Some(u64::MAX), 0, "1.21.1", true),
            None
        );
    }

    #[test]
    fn recent_oom_suggestion_fails_silent_for_missing_invalid_or_overflowing_facts() {
        for suggestion in [
            suggested_max_memory_after_recent_oom(1024, None, 0, "1.21.1", false),
            suggested_max_memory_after_recent_oom(0, Some(16_384), 0, "1.21.1", false),
            suggested_max_memory_after_recent_oom(-1, Some(16_384), 0, "1.21.1", false),
            suggested_max_memory_after_recent_oom(1024, Some(16_384), 0, " ", false),
            suggested_max_memory_after_recent_oom(
                1024,
                Some(u64::MAX - 1),
                u64::MAX,
                "1.21.1",
                true,
            ),
            suggested_max_memory_after_recent_oom(i32::MAX, Some(u64::MAX), 0, "1.21.1", true),
        ] {
            assert_eq!(suggestion, None);
        }
    }

    fn test_instance(version_id: &str) -> Instance {
        Instance {
            id: "test-instance".to_string(),
            name: "Test Instance".to_string(),
            version_id: version_id.to_string(),
            created_at: "2026-05-30T00:00:00Z".to_string(),
            last_played_at: String::new(),
            art_seed: 0,
            max_memory_mb: 0,
            min_memory_mb: 0,
            java_path: String::new(),
            window_width: 0,
            window_height: 0,
            jvm_preset: String::new(),
            performance_mode: String::new(),
            extra_jvm_args: String::new(),
            auto_optimize: false,
            icon: String::new(),
            accent: String::new(),
        }
    }

    fn test_loader_version(id: &str, inherits_from: &str) -> VersionEntry {
        VersionEntry {
            subject_kind: axial_minecraft::VersionSubjectKind::InstalledVersion,
            id: id.to_string(),
            raw_kind: "release".to_string(),
            release_time: String::new(),
            minecraft_meta: axial_minecraft::MinecraftVersionMeta::default(),
            lifecycle: axial_minecraft::LifecycleMeta::default(),
            inherits_from: inherits_from.to_string(),
            launchable: true,
            installed: true,
            status: "ready".to_string(),
            status_detail: String::new(),
            needs_install: String::new(),
            java_component: String::new(),
            java_major: 21,
            loader: Some(axial_minecraft::VersionLoaderAttachment {
                component_id: axial_minecraft::LoaderComponentId::Fabric,
                component_name: "Fabric".to_string(),
                build_id: build_id_for(
                    axial_minecraft::LoaderComponentId::Fabric,
                    "1.21.1",
                    "0.16.10",
                ),
                loader_version: "0.16.10".to_string(),
                build_meta: axial_minecraft::LoaderBuildMetadata::default(),
            }),
        }
    }
}
