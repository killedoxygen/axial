use crate::health::BundleHealth;
use crate::types::{
    CompositionPlan, CompositionTier, ManagedMod, ModCondition, OwnershipClass, PerformanceMode,
    VersionFamily,
};
use serde::{Deserialize, Serialize};

const MAX_PUBLIC_SUMMARY_CHARS: usize = 180;
const MAX_PUBLIC_DETAIL_CHARS: usize = 240;
const MAX_PUBLIC_TOKEN_CHARS: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePerformancePlan {
    pub active: bool,
    pub selected_mode: PerformanceMode,
    pub version_family: VersionFamily,
    pub loader: String,
    pub loader_posture: EffectiveLoaderPosture,
    pub composition: EffectivePerformanceComposition,
    pub managed_artifacts: Vec<EffectiveManagedArtifact>,
    pub jvm_contribution: EffectiveJvmContribution,
    pub launch_smoothing: EffectiveLaunchSmoothing,
    pub instrumentation_policy: EffectiveInstrumentationPolicy,
    pub fallback: EffectiveFallbackPlan,
    pub health_requirements: EffectivePerformanceHealthRequirements,
    pub explanation: EffectivePerformanceExplanation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveLoaderPosture {
    Vanilla,
    ModdedLoader,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePerformanceComposition {
    pub id: Option<String>,
    pub tier: CompositionTier,
    pub selected: bool,
    pub managed_artifact_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveManagedArtifact {
    pub artifact_id: String,
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub condition: ModCondition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveJvmContribution {
    pub preset: Option<String>,
    pub source: EffectiveContributionSource,
    pub explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveContributionSource {
    PerformancePlan,
    LauncherPolicy,
    UserControlled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveLaunchSmoothing {
    pub policy: EffectiveLaunchSmoothingPolicy,
    pub explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveLaunchSmoothingPolicy {
    Managed,
    LauncherDefaults,
    UserControlled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveInstrumentationPolicy {
    pub policy: EffectiveInstrumentationMode,
    pub explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveInstrumentationMode {
    NotConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveFallbackPlan {
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePerformanceHealthRequirements {
    pub expected_health: BundleHealth,
    pub expected_tier: CompositionTier,
    pub requires_composition_lock: bool,
    pub expected_managed_artifact_count: usize,
    pub managed_artifact_integrity_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_ownership: Option<OwnershipClass>,
    pub rollback_expected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePerformanceExplanation {
    pub summary: String,
    pub details: Vec<String>,
}

pub fn effective_performance_plan(plan: &CompositionPlan) -> EffectivePerformancePlan {
    let active = matches!(plan.mode, PerformanceMode::Managed);
    let managed_artifacts = plan
        .mods
        .iter()
        .map(effective_managed_artifact)
        .collect::<Vec<_>>();
    let raw_fallback_selected = !plan.fallback_reason.trim().is_empty();
    let fallback_reason = if raw_fallback_selected {
        public_text(&plan.fallback_reason, MAX_PUBLIC_DETAIL_CHARS)
            .or_else(|| Some("A safer performance fallback was selected.".to_string()))
    } else {
        None
    };

    EffectivePerformancePlan {
        active,
        selected_mode: plan.mode,
        version_family: plan.family,
        loader: public_token(&plan.loader, "vanilla"),
        loader_posture: loader_posture(&plan.loader),
        composition: EffectivePerformanceComposition {
            id: public_optional_token(&plan.composition_id),
            tier: plan.tier,
            selected: !plan.composition_id.trim().is_empty(),
            managed_artifact_count: managed_artifacts.len(),
        },
        managed_artifacts,
        jvm_contribution: jvm_contribution(plan),
        launch_smoothing: launch_smoothing(plan.mode),
        instrumentation_policy: EffectiveInstrumentationPolicy {
            policy: EffectiveInstrumentationMode::NotConfigured,
            explanation: "No performance instrumentation is required for this plan.".to_string(),
        },
        fallback: EffectiveFallbackPlan {
            selected: raw_fallback_selected,
            reason: fallback_reason.clone(),
        },
        health_requirements: EffectivePerformanceHealthRequirements {
            expected_health: BundleHealth::Healthy,
            expected_tier: plan.tier,
            requires_composition_lock: active,
            expected_managed_artifact_count: plan.mods.len(),
            managed_artifact_integrity_required: active && !plan.mods.is_empty(),
            required_ownership: (active && !plan.mods.is_empty())
                .then_some(OwnershipClass::CompositionManaged),
            rollback_expected: active && !plan.mods.is_empty(),
        },
        explanation: explanation(plan, fallback_reason.as_deref()),
    }
}

fn effective_managed_artifact(managed_mod: &ManagedMod) -> EffectiveManagedArtifact {
    EffectiveManagedArtifact {
        artifact_id: public_token(&managed_mod.artifact_id, "managed_artifact"),
        project_id: public_token(&managed_mod.project_id, "managed_project"),
        slug: public_token(&managed_mod.slug, "managed_mod"),
        name: public_label(&managed_mod.name, "Managed performance mod"),
        condition: managed_mod.condition,
    }
}

fn loader_posture(loader: &str) -> EffectiveLoaderPosture {
    if loader.trim().eq_ignore_ascii_case("vanilla") || loader.trim().is_empty() {
        EffectiveLoaderPosture::Vanilla
    } else {
        EffectiveLoaderPosture::ModdedLoader
    }
}

fn jvm_contribution(plan: &CompositionPlan) -> EffectiveJvmContribution {
    let preset = public_optional_token(&plan.jvm_preset);
    match plan.mode {
        PerformanceMode::Managed if preset.is_some() => EffectiveJvmContribution {
            preset,
            source: EffectiveContributionSource::PerformancePlan,
            explanation: "Performance contributes a JVM preset for this plan.".to_string(),
        },
        PerformanceMode::Managed => EffectiveJvmContribution {
            preset: None,
            source: EffectiveContributionSource::LauncherPolicy,
            explanation:
                "Performance does not add a JVM preset; launch uses the selected launcher policy."
                    .to_string(),
        },
        PerformanceMode::Vanilla => EffectiveJvmContribution {
            preset: None,
            source: EffectiveContributionSource::LauncherPolicy,
            explanation: "Vanilla mode leaves JVM policy to launcher settings.".to_string(),
        },
        PerformanceMode::Custom => EffectiveJvmContribution {
            preset: None,
            source: EffectiveContributionSource::UserControlled,
            explanation: "Custom mode leaves JVM policy under user control.".to_string(),
        },
    }
}

fn launch_smoothing(mode: PerformanceMode) -> EffectiveLaunchSmoothing {
    match mode {
        PerformanceMode::Managed => EffectiveLaunchSmoothing {
            policy: EffectiveLaunchSmoothingPolicy::Managed,
            explanation: "Managed mode can use launcher-side smoothing around the selected plan."
                .to_string(),
        },
        PerformanceMode::Vanilla => EffectiveLaunchSmoothing {
            policy: EffectiveLaunchSmoothingPolicy::LauncherDefaults,
            explanation: "Vanilla mode uses launcher defaults without managed performance files."
                .to_string(),
        },
        PerformanceMode::Custom => EffectiveLaunchSmoothing {
            policy: EffectiveLaunchSmoothingPolicy::UserControlled,
            explanation: "Custom mode keeps performance changes explicit and user controlled."
                .to_string(),
        },
    }
}

fn explanation(
    plan: &CompositionPlan,
    fallback_reason: Option<&str>,
) -> EffectivePerformanceExplanation {
    let mut details = Vec::new();
    let summary = match plan.mode {
        PerformanceMode::Managed if plan.mods.is_empty() => {
            "Managed performance uses launcher tuning for this version.".to_string()
        }
        PerformanceMode::Managed => format!(
            "Managed performance selected {} with {} managed performance mod{}.",
            tier_label(plan.tier),
            plan.mods.len(),
            if plan.mods.len() == 1 { "" } else { "s" }
        ),
        PerformanceMode::Vanilla => {
            "Vanilla performance mode keeps managed performance files disabled.".to_string()
        }
        PerformanceMode::Custom => {
            "Custom performance mode keeps performance choices under user control.".to_string()
        }
    };
    if let Some(reason) = fallback_reason {
        details.push(reason.to_string());
    }
    match plan.mode {
        PerformanceMode::Managed if plan.mods.is_empty() => details.push(
            "The plan remains launchable without installing managed performance mods.".to_string(),
        ),
        PerformanceMode::Managed => details.push(format!(
            "Expected managed state is {} with composition-owned artifacts.",
            tier_label(plan.tier)
        )),
        PerformanceMode::Vanilla => details.push(
            "Axial can still apply normal launch safety checks outside the performance bundle."
                .to_string(),
        ),
        PerformanceMode::Custom => details.push(
            "Axial will expose the effective result without silently changing explicit choices."
                .to_string(),
        ),
    }

    EffectivePerformanceExplanation {
        summary: public_label(&summary, "Performance plan resolved."),
        details: details
            .into_iter()
            .filter_map(|detail| public_text(&detail, MAX_PUBLIC_DETAIL_CHARS))
            .collect(),
    }
}

fn tier_label(tier: CompositionTier) -> &'static str {
    match tier {
        CompositionTier::Extended => "the extended bundle",
        CompositionTier::Core => "the core bundle",
        CompositionTier::VanillaEnhanced => "launcher tuning",
    }
}

fn public_label(value: &str, fallback: &str) -> String {
    public_text(value, MAX_PUBLIC_SUMMARY_CHARS).unwrap_or_else(|| fallback.to_string())
}

fn public_optional_token(value: &str) -> Option<String> {
    let token = public_token(value, "");
    (!token.is_empty()).then_some(token)
}

fn public_token(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if looks_sensitive_public_text(trimmed) {
        return fallback.to_string();
    }
    let token = trimmed
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '+')
        })
        .take(MAX_PUBLIC_TOKEN_CHARS)
        .collect::<String>();
    if token.is_empty() {
        fallback.to_string()
    } else {
        token
    }
}

fn public_text(value: &str, max_chars: usize) -> Option<String> {
    let text = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string();
    if looks_sensitive_public_text(&text) {
        return None;
    }
    (!text.is_empty()).then_some(text)
}

fn looks_sensitive_public_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.contains('/')
        || value.contains('\\')
        || value.contains('{')
        || value.contains('}')
        || lower.contains("://")
        || contains_command_argument(&lower)
        || lower.contains("token")
        || lower.contains("account")
        || lower.contains("username")
        || lower.contains("users")
}

fn contains_command_argument(lower: &str) -> bool {
    lower
        .split_whitespace()
        .any(|part| part.starts_with("--") || part.starts_with("-x") || part.starts_with("-d"))
}

#[cfg(test)]
mod tests {
    use super::{
        EffectiveContributionSource, EffectiveLaunchSmoothingPolicy, EffectiveLoaderPosture,
        effective_performance_plan,
    };
    use crate::health::BundleHealth;
    use crate::types::{
        CompositionPlan, CompositionTier, HardwareRequirement, ManagedMod, ModCondition,
        OwnershipClass, PerformanceMode, VersionFamily,
    };

    #[test]
    fn managed_effective_plan_covers_contract_shape() {
        let plan = CompositionPlan {
            composition_id: "family-f-fabric-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![managed_mod("sodium", "Sodium")],
            jvm_preset: "performance".to_string(),
            warnings: Vec::new(),
            fallback_reason: "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option.".to_string(),
        };

        let effective = effective_performance_plan(&plan);

        assert!(effective.active);
        assert_eq!(effective.selected_mode, PerformanceMode::Managed);
        assert_eq!(effective.version_family, VersionFamily::F);
        assert_eq!(
            effective.loader_posture,
            EffectiveLoaderPosture::ModdedLoader
        );
        assert_eq!(
            effective.composition.id.as_deref(),
            Some("family-f-fabric-extended")
        );
        assert_eq!(effective.composition.managed_artifact_count, 1);
        assert_eq!(effective.managed_artifacts[0].slug, "sodium");
        assert_eq!(
            effective.jvm_contribution.source,
            EffectiveContributionSource::PerformancePlan
        );
        assert_eq!(
            effective.launch_smoothing.policy,
            EffectiveLaunchSmoothingPolicy::Managed
        );
        assert!(effective.fallback.selected);
        assert_eq!(
            effective.health_requirements.required_ownership,
            Some(OwnershipClass::CompositionManaged)
        );
        assert_eq!(
            effective.health_requirements.expected_health,
            BundleHealth::Healthy
        );
        assert!(
            effective
                .explanation
                .summary
                .starts_with("Managed performance selected")
        );
        assert!(
            effective
                .explanation
                .details
                .iter()
                .any(|detail| detail.contains("temporarily unavailable"))
        );
    }

    #[test]
    fn vanilla_and_custom_effective_plans_disable_managed_artifact_policy() {
        for mode in [PerformanceMode::Vanilla, PerformanceMode::Custom] {
            let plan = CompositionPlan {
                composition_id: String::new(),
                family: VersionFamily::E,
                loader: "vanilla".to_string(),
                mode,
                tier: CompositionTier::VanillaEnhanced,
                mods: Vec::new(),
                jvm_preset: String::new(),
                warnings: Vec::new(),
                fallback_reason: String::new(),
            };

            let effective = effective_performance_plan(&plan);

            assert!(!effective.active);
            assert_eq!(effective.loader_posture, EffectiveLoaderPosture::Vanilla);
            assert_eq!(effective.composition.id, None);
            assert!(effective.managed_artifacts.is_empty());
            assert_eq!(effective.health_requirements.required_ownership, None);
            assert!(!effective.health_requirements.rollback_expected);
            assert!(
                !effective
                    .health_requirements
                    .managed_artifact_integrity_required
            );
        }
    }

    #[test]
    fn effective_plan_bounds_public_explanations_and_artifact_labels() {
        let long_text = format!("{}\n{}", "unsafe detail ".repeat(40), "x".repeat(400));
        let plan = CompositionPlan {
            composition_id: "family-f-fabric-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![managed_mod("bad id with spaces / path", &long_text)],
            jvm_preset: "performance with spaces".to_string(),
            warnings: vec![long_text.clone()],
            fallback_reason: long_text,
        };

        let effective = effective_performance_plan(&plan);

        assert!(effective.explanation.summary.len() <= 180);
        assert!(
            effective
                .explanation
                .details
                .iter()
                .all(|detail| detail.len() <= 240 && !detail.contains('\n'))
        );
        assert!(effective.managed_artifacts[0].name.len() <= 180);
        assert_eq!(
            effective.managed_artifacts[0].artifact_id,
            "managed_artifact"
        );
        assert_eq!(
            effective.jvm_contribution.preset.as_deref(),
            Some("performancewithspaces")
        );
    }

    #[test]
    fn effective_plan_replaces_suspicious_public_text() {
        let plan = CompositionPlan {
            composition_id: "family-f-fabric-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![managed_mod("sodium", "/home/alice/.minecraft --token secret")],
            jvm_preset: "performance".to_string(),
            warnings: Vec::new(),
            fallback_reason:
                r#"C:\Users\Alice\AppData\Roaming\.minecraft --accessToken secret {"provider":"payload"}"#
                    .to_string(),
        };

        let effective = effective_performance_plan(&plan);
        let encoded = serde_json::to_string(&effective).expect("effective json");
        let lower = encoded.to_ascii_lowercase();

        assert!(effective.fallback.selected);
        assert_eq!(
            effective.fallback.reason.as_deref(),
            Some("A safer performance fallback was selected.")
        );
        assert_eq!(
            effective.managed_artifacts[0].name,
            "Managed performance mod"
        );
        assert!(!lower.contains("users"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("provider"));
        assert!(!lower.contains("minecraft"));
    }

    fn managed_mod(id: &str, name: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: id.to_string(),
            project_id: id.to_string(),
            slug: id.to_string(),
            name: name.to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: Some(HardwareRequirement::default()),
            mutual_exclusions: Vec::new(),
        }
    }
}
