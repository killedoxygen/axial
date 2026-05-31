use super::LaunchHealingSummary;
use crate::healing::{HealingEvent, HealingEventKind};
use crate::types::LaunchFailureClass;

pub struct HealingSummaryInput<'a> {
    pub auth_mode: &'a str,
    pub requested_java_path: &'a str,
    pub requested_preset: &'a str,
    pub effective_java_path: Option<&'a str>,
    pub effective_preset: Option<&'a str>,
    pub fallback_applied: Option<&'a str>,
    pub retry_count: u32,
    pub failure_class: Option<LaunchFailureClass>,
}

pub fn build_healing_summary(input: HealingSummaryInput<'_>) -> Option<LaunchHealingSummary> {
    let requested_java_path = input.requested_java_path.trim();
    let requested_preset = (!input.requested_preset.trim().is_empty())
        .then(|| input.requested_preset.trim().to_string());
    let effective_java_path = input
        .effective_java_path
        .filter(|value| !value.trim().is_empty())
        .map(str::trim);
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
    let auth_mode = match input.auth_mode.trim() {
        "online" => "online",
        _ => "offline",
    };

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
    if let Some(effective) = effective_java_path
        && !requested_java_path.is_empty()
        && requested_java_path != effective
    {
        let detail = "Requested runtime override was bypassed in favor of a safer managed runtime"
            .to_string();
        warnings.push(detail.clone());
        events.push(HealingEvent {
            kind: HealingEventKind::RuntimeBypassed,
            detail: Some(detail),
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
        auth_mode: Some(auth_mode.to_string()),
        warnings,
        fallback_applied,
        retry_count: (input.retry_count > 0).then_some(input.retry_count),
        failure_class: failure_class_name,
        events,
    };

    if summary.requested_preset.is_none()
        && summary.effective_preset.is_none()
        && summary.warnings.is_empty()
        && summary.fallback_applied.is_none()
        && summary.retry_count.is_none()
        && summary.failure_class.is_none()
        && summary.events.is_empty()
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

#[cfg(test)]
mod tests {
    use super::{HealingSummaryInput, build_healing_summary};
    use serde_json::Value;

    #[test]
    fn ignores_runtime_bypass_when_java_paths_only_differ_by_whitespace() {
        let summary = build_healing_summary(HealingSummaryInput {
            auth_mode: "offline",
            requested_java_path: " /usr/bin/java ",
            requested_preset: "",
            effective_java_path: Some("/usr/bin/java"),
            effective_preset: None,
            fallback_applied: None,
            retry_count: 0,
            failure_class: None,
        });

        assert!(summary.is_none());
    }

    #[test]
    fn runtime_bypass_summary_serializes_without_java_path_fragments() {
        let summary = build_healing_summary(HealingSummaryInput {
            auth_mode: "offline",
            requested_java_path: " /home/alice/.sdkman/candidates/java/21/bin/java ",
            requested_preset: "",
            effective_java_path: Some(r"C:\Users\alice\AppData\Local\VendorRuntime\java.exe"),
            effective_preset: None,
            fallback_applied: None,
            retry_count: 0,
            failure_class: None,
        })
        .expect("expected runtime bypass healing summary");

        assert!(summary.events.iter().any(|event| matches!(
            event.kind,
            crate::healing::HealingEventKind::RuntimeBypassed
        )));

        let serialized = serde_json::to_string(&summary).expect("serialize healing summary");
        let serialized_lower = serialized.to_ascii_lowercase();
        for fragment in [
            "/usr",
            "/home",
            "\\",
            "java",
            "alice",
            "sdkman",
            "candidates",
            "bin",
            "users",
            "appdata",
            "vendorruntime",
            "java.exe",
        ] {
            assert!(
                !serialized_lower.contains(fragment),
                "serialized healing leaked fragment {fragment:?}: {serialized}"
            );
        }

        let value: Value = serde_json::from_str(&serialized).expect("parse healing summary");
        assert!(value.get("requested_java_path").is_none());
        assert!(value.get("effective_java_path").is_none());
    }
}
