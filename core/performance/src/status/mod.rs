use crate::health::BundleHealth;
use crate::rules_cache::RulesCacheStatus;
use crate::types::{
    CompositionTier, EmergencyDisableTarget, Manifest, OwnershipClass, VersionFamily,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    BuiltIn,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleChannel {
    Bundled,
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulesValidation {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceRulesStatus {
    pub rule_source: RuleSource,
    pub rule_channel: RuleChannel,
    pub rules_cache: RulesCacheStatus,
    pub schema_version: i32,
    pub generated_at: String,
    pub composition_count: usize,
    pub family_coverage: Vec<FamilyCoverage>,
    pub remote_refresh: bool,
    pub last_refresh_at: Option<String>,
    pub validation: RulesValidation,
    pub health_states: Vec<BundleHealth>,
    pub ownership_classes: Vec<OwnershipClass>,
    pub emergency_disable_count: usize,
    pub emergency_disables: Vec<EmergencyDisableDiagnostic>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyCoverage {
    pub family: VersionFamily,
    pub composition_count: usize,
    pub loaders: Vec<String>,
    pub tiers: Vec<CompositionTier>,
    pub managed_mod_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmergencyDisableDiagnostic {
    pub id: String,
    pub target: EmergencyDisableTarget,
    pub target_id: String,
    pub reason: String,
    pub families: Vec<VersionFamily>,
    pub loaders: Vec<String>,
    pub tiers: Vec<CompositionTier>,
}

pub fn rules_status(manifest: &Manifest) -> PerformanceRulesStatus {
    rules_status_with_cache(manifest, RulesCacheStatus::unavailable())
}

pub fn rules_status_with_cache(
    manifest: &Manifest,
    rules_cache: RulesCacheStatus,
) -> PerformanceRulesStatus {
    rules_status_for(
        manifest,
        RuleSource::BuiltIn,
        RuleChannel::Bundled,
        rules_cache,
        false,
        None,
        RulesValidation::Valid,
    )
}

pub fn rules_status_for(
    manifest: &Manifest,
    rule_source: RuleSource,
    rule_channel: RuleChannel,
    rules_cache: RulesCacheStatus,
    remote_refresh: bool,
    last_refresh_at: Option<String>,
    validation: RulesValidation,
) -> PerformanceRulesStatus {
    let warnings = rules_cache
        .warning
        .clone()
        .map(|warning| vec![warning])
        .unwrap_or_default();

    PerformanceRulesStatus {
        rule_source,
        rule_channel,
        rules_cache,
        schema_version: manifest.schema_version,
        generated_at: manifest.generated_at.clone(),
        composition_count: manifest.compositions.len(),
        family_coverage: family_coverage(manifest),
        remote_refresh,
        last_refresh_at,
        validation,
        health_states: vec![
            BundleHealth::Healthy,
            BundleHealth::Degraded,
            BundleHealth::Fallback,
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
        warnings,
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
    use super::{RuleChannel, RuleSource, RulesValidation, rules_status, rules_status_with_cache};
    use crate::health::BundleHealth;
    use crate::resolve::builtin_manifest;
    use crate::rules_cache::RulesCacheState;
    use crate::types::{EmergencyDisable, EmergencyDisableTarget, OwnershipClass, VersionFamily};

    #[test]
    fn bundled_manifest_status_is_truthful_about_current_foundation() {
        let manifest = builtin_manifest().expect("builtin manifest should validate");

        let status = rules_status(&manifest);

        assert_eq!(status.rule_source, RuleSource::BuiltIn);
        assert_eq!(status.rule_channel, RuleChannel::Bundled);
        assert!(!status.rules_cache.recorded);
        assert_eq!(status.rules_cache.state, RulesCacheState::Unavailable);
        assert_eq!(status.schema_version, manifest.schema_version);
        assert_eq!(status.generated_at, manifest.generated_at);
        assert_eq!(status.composition_count, manifest.compositions.len());
        assert_eq!(status.family_coverage.len(), 6);
        assert!(!status.remote_refresh);
        assert_eq!(status.last_refresh_at, None);
        assert_eq!(status.validation, RulesValidation::Valid);
        assert_eq!(
            status.health_states,
            vec![
                BundleHealth::Healthy,
                BundleHealth::Degraded,
                BundleHealth::Fallback,
                BundleHealth::Disabled,
                BundleHealth::Invalid,
            ]
        );
        assert_eq!(
            status.ownership_classes,
            vec![
                OwnershipClass::CompositionManaged,
                OwnershipClass::UserManaged,
            ]
        );
        assert_eq!(status.emergency_disable_count, 0);
        assert!(status.emergency_disables.is_empty());
        assert!(status.warnings.is_empty());
    }

    #[test]
    fn status_exposes_rules_cache_diagnostics() {
        let manifest = builtin_manifest().expect("builtin manifest should validate");

        let status = rules_status_with_cache(
            &manifest,
            crate::rules_cache::RulesCacheStatus {
                recorded: true,
                state: RulesCacheState::Recovered,
                updated_at: Some("2026-05-30T10:00:00Z".to_string()),
                loaded_at: Some("2026-05-30T10:01:00Z".to_string()),
                warning: Some("Rules cache was invalid and was replaced.".to_string()),
            },
        );

        assert!(status.rules_cache.recorded);
        assert_eq!(status.rules_cache.state, RulesCacheState::Recovered);
        assert_eq!(
            status.rules_cache.updated_at.as_deref(),
            Some("2026-05-30T10:00:00Z")
        );
        assert_eq!(
            status.rules_cache.loaded_at.as_deref(),
            Some("2026-05-30T10:01:00Z")
        );
        assert!(status.rules_cache.warning.is_some());
    }

    #[test]
    fn status_exposes_emergency_disable_diagnostics() {
        let mut manifest = builtin_manifest().expect("builtin manifest should validate");
        manifest.emergency_disables.push(EmergencyDisable {
            id: "disable-family-f-extended".to_string(),
            target: EmergencyDisableTarget::Composition,
            target_id: "family-f-fabric-extended".to_string(),
            reason: "Temporary compatibility hold.".to_string(),
            families: vec![VersionFamily::F],
            loaders: vec!["fabric".to_string()],
            tiers: Vec::new(),
        });

        let status = rules_status(&manifest);

        assert_eq!(status.emergency_disable_count, 1);
        assert_eq!(status.emergency_disables[0].id, "disable-family-f-extended");
        assert_eq!(
            status.emergency_disables[0].target,
            EmergencyDisableTarget::Composition
        );
        assert_eq!(
            status.emergency_disables[0].reason,
            "Temporary compatibility hold."
        );
    }

    #[test]
    fn bundled_manifest_status_reports_all_family_coverage() {
        let manifest = builtin_manifest().expect("builtin manifest should validate");

        let status = rules_status(&manifest);
        let families = status
            .family_coverage
            .iter()
            .map(|coverage| coverage.family)
            .collect::<Vec<_>>();

        assert_eq!(
            families,
            vec![
                VersionFamily::A,
                VersionFamily::B,
                VersionFamily::C,
                VersionFamily::D,
                VersionFamily::E,
                VersionFamily::F,
            ]
        );
        assert!(
            status
                .family_coverage
                .iter()
                .all(|coverage| coverage.composition_count > 0)
        );
    }

    #[test]
    fn vanilla_enhanced_only_families_are_warned_without_invalidating_status() {
        let manifest = builtin_manifest().expect("builtin manifest should validate");

        let status = rules_status(&manifest);

        for family in [
            VersionFamily::A,
            VersionFamily::B,
            VersionFamily::C,
            VersionFamily::D,
        ] {
            let coverage = status
                .family_coverage
                .iter()
                .find(|coverage| coverage.family == family)
                .expect("family coverage");
            assert_eq!(coverage.managed_mod_count, 0);
            assert!(
                coverage
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("intentionally vanilla-enhanced only")),
                "missing vanilla-enhanced-only warning for {family:?}"
            );
        }

        for family in [VersionFamily::E, VersionFamily::F] {
            let coverage = status
                .family_coverage
                .iter()
                .find(|coverage| coverage.family == family)
                .expect("family coverage");
            assert!(coverage.managed_mod_count > 0);
            assert!(
                coverage.warnings.is_empty(),
                "managed-covered family should not warn: {family:?}"
            );
        }
    }
}
