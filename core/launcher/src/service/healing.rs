use super::{LaunchHealingSummary, RecoveryAction, RecoveryPlan};
use crate::healing::{HealingEvent, HealingEventKind};
use crate::types::LaunchFailureClass;
use croopor_minecraft::JavaRuntimeInfo;

pub struct HealingSummaryInput<'a> {
    pub requested_java_path: &'a str,
    pub requested_preset: &'a str,
    pub effective_java_path: Option<&'a str>,
    pub effective_preset: Option<&'a str>,
    pub advanced_overrides: bool,
    pub fallback_applied: Option<&'a str>,
    pub retry_count: u32,
    pub failure_class: Option<LaunchFailureClass>,
}

pub fn build_healing_summary(input: HealingSummaryInput<'_>) -> Option<LaunchHealingSummary> {
    let requested_java_path = (!input.requested_java_path.trim().is_empty())
        .then(|| input.requested_java_path.trim().to_string());
    let requested_preset = (!input.requested_preset.trim().is_empty())
        .then(|| input.requested_preset.trim().to_string());
    let effective_java_path = input
        .effective_java_path
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string());
    let effective_preset = input
        .effective_preset
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string());
    let fallback_applied = input
        .fallback_applied
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let failure_class_name = input
        .failure_class
        .map(super::mapping::failure_class_name)
        .map(str::to_string);

    let mut warnings = Vec::new();
    let mut events = Vec::new();

    if let Some(requested) = requested_preset.as_ref() {
        let effective = effective_preset.as_deref().unwrap_or("none");
        if requested != effective {
            let detail = format!(
                "Requested JVM preset \"{requested}\" was downgraded to \"{effective}\" for compatibility"
            );
            warnings.push(detail.clone());
            events.push(HealingEvent {
                kind: HealingEventKind::PresetDowngraded,
                detail: Some(detail),
            });
        }
    }
    if let (Some(requested), Some(effective)) =
        (requested_java_path.as_ref(), effective_java_path.as_ref())
        && requested != effective
    {
        let detail =
            "Requested Java override was bypassed in favor of a safer managed runtime".to_string();
        warnings.push(detail.clone());
        events.push(HealingEvent {
            kind: HealingEventKind::RuntimeBypassed,
            detail: Some(format!("requested={requested} effective={effective}")),
        });
    }
    if let Some(detail) = fallback_applied.as_ref() {
        events.push(HealingEvent {
            kind: HealingEventKind::FallbackApplied,
            detail: Some(detail.clone()),
        });
    }
    if matches!(
        input.failure_class,
        Some(LaunchFailureClass::StartupStalled)
    ) {
        events.push(HealingEvent {
            kind: HealingEventKind::StartupStalled,
            detail: Some("no startup activity observed".to_string()),
        });
    }

    let summary = LaunchHealingSummary {
        requested_preset,
        effective_preset,
        requested_java_path,
        effective_java_path,
        auth_mode: Some("offline".to_string()),
        warnings,
        fallback_applied,
        retry_count: (input.retry_count > 0).then_some(input.retry_count),
        failure_class: failure_class_name,
        advanced_overrides: Some(input.advanced_overrides),
        events,
    };

    if summary.requested_preset.is_none()
        && summary.effective_preset.is_none()
        && summary.requested_java_path.is_none()
        && summary.effective_java_path.is_none()
        && summary.warnings.is_empty()
        && summary.fallback_applied.is_none()
        && summary.retry_count.is_none()
        && summary.failure_class.is_none()
        && summary.events.is_empty()
        && !input.advanced_overrides
    {
        None
    } else {
        Some(summary)
    }
}

pub fn infer_loader(version_id: &str) -> &'static str {
    let version = version_id.to_ascii_lowercase();
    if version.contains("neoforge") {
        "neoforge"
    } else if version.contains("fabric") {
        "fabric"
    } else if version.contains("forge") {
        "forge"
    } else if version.contains("quilt") {
        "quilt"
    } else {
        "vanilla"
    }
}

pub fn recovery_for_failure(
    class: LaunchFailureClass,
    version_id: &str,
    info: &JavaRuntimeInfo,
    requested_java: &str,
    advanced_overrides: bool,
    disable_custom_gc: bool,
    effective_preset: &str,
) -> Option<RecoveryPlan> {
    if advanced_overrides {
        return None;
    }

    match class {
        LaunchFailureClass::JvmUnsupportedOption
        | LaunchFailureClass::JvmExperimentalUnlock
        | LaunchFailureClass::JvmOptionOrdering => {
            if !effective_preset.trim().is_empty() {
                let preset = conservative_healing_preset(version_id, info);
                if !preset.is_empty() && preset != effective_preset {
                    return Some(RecoveryPlan {
                        description: format!(
                            "Automatic retry: downgraded JVM preset to \"{preset}\" after startup failure"
                        ),
                        action: RecoveryAction::DowngradePreset(preset),
                    });
                }
            }
            if !disable_custom_gc {
                return Some(RecoveryPlan {
                    description: "Automatic retry: disabled custom GC flags after startup failure"
                        .to_string(),
                    action: RecoveryAction::DisableCustomGc,
                });
            }
        }
        LaunchFailureClass::JavaRuntimeMismatch => {
            if !requested_java.trim().is_empty() {
                return Some(RecoveryPlan {
                    description: "Automatic retry: switched to managed Java after runtime mismatch"
                        .to_string(),
                    action: RecoveryAction::SwitchManagedRuntime,
                });
            }
        }
        _ => {}
    }
    None
}

pub fn conservative_healing_preset(version_id: &str, info: &JavaRuntimeInfo) -> String {
    if info.major <= 8 || is_legacy_version_family(version_id) {
        "legacy".to_string()
    } else {
        "performance".to_string()
    }
}

fn is_legacy_version_family(version_id: &str) -> bool {
    let base = version_id.split("-forge-").next().unwrap_or(version_id);
    let numbers = base
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    matches!(numbers.as_slice(), [1, minor, ..] if *minor <= 12)
}

#[cfg(test)]
mod tests {
    use super::{HealingSummaryInput, build_healing_summary};

    #[test]
    fn ignores_runtime_bypass_when_java_paths_only_differ_by_whitespace() {
        let summary = build_healing_summary(HealingSummaryInput {
            requested_java_path: " /usr/bin/java ",
            requested_preset: "",
            effective_java_path: Some("/usr/bin/java"),
            effective_preset: None,
            advanced_overrides: false,
            fallback_applied: None,
            retry_count: 0,
            failure_class: None,
        })
        .expect("expected healing summary");

        assert_eq!(
            summary.requested_java_path.as_deref(),
            Some("/usr/bin/java")
        );
        assert_eq!(
            summary.effective_java_path.as_deref(),
            Some("/usr/bin/java")
        );
        assert!(summary.warnings.is_empty());
        assert!(summary.events.iter().all(|event| !matches!(
            event.kind,
            crate::healing::HealingEventKind::RuntimeBypassed
        )));
    }
}
