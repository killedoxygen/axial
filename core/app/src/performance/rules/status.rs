//! Public status projection of the active rules.

use super::ActiveRules;
use crate::performance::model::{
    BundleHealth, CompositionTier, EmergencyDisableDiagnostic, FamilyCoverage, Manifest,
    OwnershipClass, PerformanceRulesStatus, PerformanceRulesStatusResponse,
    PerformanceRulesStatusViewModel, RuleChannel, RuleSource, RulesCacheState, RulesValidation,
    VersionFamily, ViewModelTone,
};
use std::collections::BTreeSet;

pub(super) fn response(
    active: &ActiveRules,
    remote_refresh: bool,
) -> PerformanceRulesStatusResponse {
    let status = rules_status(active, remote_refresh);
    let valid = status.validation == RulesValidation::Valid
        && status.rules_cache.state != RulesCacheState::Invalid;
    let view_model = PerformanceRulesStatusViewModel {
        source_label: match status.rule_source {
            RuleSource::BuiltIn => "Built-in rules",
            RuleSource::Remote => "Remote rules",
        }
        .into(),
        channel_label: match status.rule_channel {
            RuleChannel::Bundled => "Bundled",
            RuleChannel::Local => "Local",
            RuleChannel::Remote => "Remote",
        }
        .into(),
        validation_label: if valid { "Valid" } else { "Needs attention" }.into(),
        validation_tone: if valid {
            ViewModelTone::Ok
        } else {
            ViewModelTone::Warn
        },
        validation_icon: if valid { "check" } else { "alert-triangle" }.into(),
        summary: format!("{} performance compositions", status.composition_count),
        refresh_label: if status.remote_refresh {
            "Refresh rules"
        } else {
            "Remote refresh is not configured"
        }
        .into(),
        generated_label: status.generated_at.clone(),
        cache_label: if status.rules_cache.recorded {
            "Cached"
        } else {
            "No verified remote cache"
        }
        .into(),
        emergency_disable_label: format!("{} emergency disables", status.emergency_disable_count),
        details_label: "Performance rules".into(),
        health_states_label: "Healthy, disabled, invalid".into(),
        ownership_label: "Composition-managed files and user-managed files".into(),
        warnings: status.warnings.clone(),
    };
    PerformanceRulesStatusResponse { status, view_model }
}

fn rules_status(active: &ActiveRules, remote_refresh: bool) -> PerformanceRulesStatus {
    let manifest = &active.manifest;
    PerformanceRulesStatus {
        rule_source: active.source,
        rule_channel: match active.source {
            RuleSource::BuiltIn => RuleChannel::Bundled,
            RuleSource::Remote => RuleChannel::Remote,
        },
        rules_cache: active.cache.clone(),
        schema_version: manifest.schema_version,
        generated_at: manifest.generated_at.clone(),
        composition_count: manifest.compositions.len(),
        family_coverage: family_coverage(manifest),
        remote_refresh,
        last_refresh_at: active.cache.updated_at.clone(),
        validation: RulesValidation::Valid,
        health_states: vec![
            BundleHealth::Healthy,
            BundleHealth::Disabled,
            BundleHealth::Invalid,
        ],
        ownership_classes: vec![
            OwnershipClass::CompositionManaged,
            OwnershipClass::UserManaged,
        ],
        emergency_disable_count: manifest.emergency_disables.len(),
        emergency_disables: manifest
            .emergency_disables
            .iter()
            .map(|disable| EmergencyDisableDiagnostic {
                id: disable.id.clone(),
                target: disable.target,
                target_id: disable.target_id.clone(),
                reason: disable.reason.clone(),
                families: disable.families.clone(),
                loaders: disable.loaders.clone(),
                tiers: disable.tiers.clone(),
            })
            .collect(),
        warnings: active.cache.warning.clone().into_iter().collect(),
    }
}

fn family_coverage(manifest: &Manifest) -> Vec<FamilyCoverage> {
    [
        VersionFamily::A,
        VersionFamily::B,
        VersionFamily::C,
        VersionFamily::D,
        VersionFamily::E,
        VersionFamily::F,
    ]
    .into_iter()
    .map(|family| coverage_for_family(manifest, family))
    .collect()
}

fn coverage_for_family(manifest: &Manifest, family: VersionFamily) -> FamilyCoverage {
    let mut composition_count = 0;
    let mut managed_mod_count = 0;
    let mut loader_set = BTreeSet::new();
    let mut raw_tiers = Vec::new();

    for composition in &manifest.compositions {
        if !composition.families.contains(&family) {
            continue;
        }

        composition_count += 1;
        managed_mod_count += composition.mods.len();
        for loader in &composition.loaders {
            loader_set.insert(loader.to_lowercase());
        }
        if !raw_tiers.contains(&composition.tier) {
            raw_tiers.push(composition.tier);
        }
    }

    let tiers = [
        CompositionTier::Extended,
        CompositionTier::Core,
        CompositionTier::VanillaEnhanced,
    ]
    .into_iter()
    .filter(|tier| raw_tiers.contains(tier))
    .collect::<Vec<_>>();

    let mut warnings = Vec::new();
    if composition_count == 0 {
        warnings.push(format!(
            "Family {family:?} has no performance composition coverage."
        ));
    } else if managed_mod_count == 0 && tiers == vec![CompositionTier::VanillaEnhanced] {
        warnings.push(format!(
            "Family {family:?} is intentionally vanilla-enhanced only; no managed performance mods are declared."
        ));
    }

    FamilyCoverage {
        family,
        composition_count,
        loaders: loader_set.into_iter().collect(),
        tiers,
        managed_mod_count,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::super::ActiveRules;
    use super::rules_status;
    use crate::performance::model::{
        CompositionTier, EmergencyDisable, EmergencyDisableTarget, Manifest,
        PerformanceRulesStatus, RuleChannel, RuleSource, RulesCacheState, RulesValidation,
        VersionFamily,
    };
    use crate::performance::resolve::builtin_manifest;

    fn status(manifest: &Manifest) -> PerformanceRulesStatus {
        rules_status(&ActiveRules::builtin(manifest.clone()), false)
    }

    #[test]
    fn bundled_status_reports_its_source_and_per_family_coverage() {
        let manifest = builtin_manifest().expect("builtin manifest should validate");

        let status = status(&manifest);

        assert_eq!(status.rule_source, RuleSource::BuiltIn);
        assert_eq!(status.rule_channel, RuleChannel::Bundled);
        assert!(!status.rules_cache.recorded);
        assert_eq!(status.rules_cache.state, RulesCacheState::Unavailable);
        assert_eq!(status.validation, RulesValidation::Valid);
        assert!(!status.remote_refresh);
        assert_eq!(status.last_refresh_at, None);
        assert_eq!(status.composition_count, manifest.compositions.len());
        assert!(status.emergency_disables.is_empty());
        assert!(status.warnings.is_empty());
        use VersionFamily::{A, B, C, D, E, F};
        assert_eq!(
            status
                .family_coverage
                .iter()
                .map(|coverage| coverage.family)
                .collect::<Vec<_>>(),
            [A, B, C, D, E, F]
        );
        for coverage in &status.family_coverage {
            let vanilla_only = matches!(coverage.family, A | B | D);
            assert!(coverage.composition_count > 0, "{:?}", coverage.family);
            assert_eq!(
                coverage.managed_mod_count == 0,
                vanilla_only,
                "{:?}",
                coverage.family
            );
            assert_eq!(
                coverage
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("intentionally vanilla-enhanced only")),
                vanilla_only,
                "{:?}",
                coverage.family
            );
            assert_eq!(coverage.warnings.is_empty(), !vanilla_only);
        }
        let family_c = &status.family_coverage[2];
        assert_eq!(family_c.managed_mod_count, 3);
        assert!(family_c.tiers.contains(&CompositionTier::Core));
        assert!(family_c.tiers.contains(&CompositionTier::VanillaEnhanced));
    }

    #[test]
    fn status_exposes_emergency_disable_diagnostics() {
        let mut manifest = builtin_manifest().expect("builtin manifest should validate");
        manifest.emergency_disables.push(EmergencyDisable {
            id: "disable-family-f-extended".to_string(),
            target: EmergencyDisableTarget::Composition,
            target_id: "family-f-fabric-extended".to_string(),
            reason: "Temporary hold.".to_string(),
            families: vec![VersionFamily::F],
            loaders: vec!["fabric".to_string()],
            tiers: Vec::new(),
        });

        let status = status(&manifest);

        assert_eq!(status.emergency_disable_count, 1);
        assert_eq!(status.emergency_disables[0].id, "disable-family-f-extended");
        assert_eq!(
            status.emergency_disables[0].target,
            EmergencyDisableTarget::Composition
        );
        assert_eq!(status.emergency_disables[0].reason, "Temporary hold.");
    }
}
