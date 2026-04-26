use croopor_config::{AppConfig, Instance};
use croopor_launcher::{GuardianMode, OverrideOrigin, SessionId};
use std::time::SystemTime;

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
) -> i32 {
    if instance.max_memory_mb > 0 {
        instance.max_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else {
        config.max_memory_mb
    }
}

pub(super) fn effective_min_memory(
    instance: &Instance,
    config: &AppConfig,
    requested: Option<i32>,
    max_memory_mb: i32,
) -> i32 {
    let min_memory_mb = if instance.min_memory_mb > 0 {
        instance.min_memory_mb
    } else if requested.unwrap_or_default() > 0 {
        requested.unwrap_or_default()
    } else {
        config.min_memory_mb
    };
    min_memory_mb.min(max_memory_mb).max(0)
}

pub(super) fn split_jvm_args(extra_jvm_args: &str) -> Vec<String> {
    shlex::split(extra_jvm_args).unwrap_or_else(|| {
        extra_jvm_args
            .split_whitespace()
            .map(str::to_string)
            .collect()
    })
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
