use super::version::{compare_release_version, parse_version, version_in_range};
use crate::types::{
    CompositionDef, CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
    HardwareProfile, ManagedMod, Manifest, ModCondition, PerformanceMode, ResolutionRequest,
    VersionFamily,
};

pub fn resolve_plan(manifest: Option<&Manifest>, request: ResolutionRequest) -> CompositionPlan {
    let family = classify_version(&request.game_version);
    let loader = normalize_loader(&request.loader);
    let mode = request.mode;

    if matches!(mode, PerformanceMode::Vanilla | PerformanceMode::Custom) {
        return CompositionPlan {
            composition_id: String::new(),
            family,
            loader,
            mode,
            tier: CompositionTier::VanillaEnhanced,
            mods: Vec::new(),
            jvm_preset: String::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };
    }

    let Some(manifest) = manifest else {
        return vanilla_enhanced_plan(family, loader, mode);
    };

    let installed_set: std::collections::HashSet<String> = request
        .installed_mods
        .iter()
        .map(|value| value.to_lowercase())
        .collect();

    let mut fallback_warnings = Vec::new();
    let mut fallback_reason = String::new();
    for tier in [
        CompositionTier::Extended,
        CompositionTier::Core,
        CompositionTier::VanillaEnhanced,
    ] {
        for definition in matching_compositions(manifest, family, &loader, tier) {
            if let Some(disable) = active_composition_disable(manifest, definition, family, &loader)
            {
                push_unique_warning(
                    &mut fallback_warnings,
                    composition_disable_warning(disable, &definition.id),
                );
                if fallback_reason.is_empty() {
                    fallback_reason = "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option.".to_string();
                }
                continue;
            }

            let mut active_mods = Vec::new();
            let mut warnings = fallback_warnings.clone();
            let mut inactive_warnings = Vec::new();
            for managed_mod in &definition.mods {
                if let Some(disable) =
                    active_artifact_disable(manifest, managed_mod, family, &loader, definition.tier)
                {
                    let warning = artifact_disable_warning(disable, managed_mod);
                    push_unique_warning(&mut warnings, warning.clone());
                    push_unique_warning(&mut inactive_warnings, warning);
                    continue;
                }

                let (include, warning) = should_include_mod(
                    managed_mod,
                    &request.game_version,
                    &request.hardware,
                    &installed_set,
                );
                if !warning.is_empty() {
                    push_unique_warning(&mut warnings, warning.clone());
                    push_unique_warning(&mut inactive_warnings, warning);
                }
                if include {
                    active_mods.push(managed_mod.clone());
                }
            }

            if active_mods.len() >= 2 || matches!(tier, CompositionTier::VanillaEnhanced) {
                let plan = CompositionPlan {
                    composition_id: definition.id.clone(),
                    family,
                    loader: loader.clone(),
                    mode,
                    tier: definition.tier,
                    mods: active_mods,
                    jvm_preset: definition.jvm_preset.clone(),
                    warnings,
                    fallback_reason: fallback_reason.clone(),
                };
                return plan;
            }

            if !matches!(tier, CompositionTier::VanillaEnhanced) {
                for warning in inactive_warnings {
                    push_unique_warning(&mut fallback_warnings, warning);
                }
                push_unique_warning(
                    &mut fallback_warnings,
                    format!(
                        "{} skipped: not enough compatible performance mods for this instance",
                        definition.id
                    ),
                );
                if fallback_reason.is_empty() {
                    fallback_reason = "A faster performance bundle is not compatible with this instance, so Axial chose a safer option.".to_string();
                }
            }
        }
    }

    let mut plan = vanilla_enhanced_plan(family, loader, mode);
    plan.warnings = fallback_warnings;
    if !plan.warnings.is_empty() {
        plan.fallback_reason = if fallback_reason.is_empty() {
            "Managed performance bundles are temporarily unavailable, so Axial will only tune launcher settings.".to_string()
        } else {
            fallback_reason
        };
    }
    plan
}

pub fn parse_mode(raw: &str) -> Option<PerformanceMode> {
    match raw.trim().to_lowercase().as_str() {
        "managed" => Some(PerformanceMode::Managed),
        "vanilla" => Some(PerformanceMode::Vanilla),
        "custom" => Some(PerformanceMode::Custom),
        _ => None,
    }
}

pub fn classify_version(mc_version: &str) -> VersionFamily {
    let Ok(version) = parse_version(mc_version) else {
        return VersionFamily::F;
    };
    if version.is_snapshot {
        return VersionFamily::F;
    }

    match compare_release_version(&version, 1, 6, 0) {
        value if value < 0 => VersionFamily::A,
        _ if compare_release_version(&version, 1, 7, 10) <= 0 => VersionFamily::B,
        _ if compare_release_version(&version, 1, 12, 2) <= 0 => VersionFamily::C,
        _ if compare_release_version(&version, 1, 15, 2) <= 0 => VersionFamily::D,
        _ if compare_release_version(&version, 1, 20, 1) <= 0 => VersionFamily::E,
        _ => VersionFamily::F,
    }
}

fn normalize_loader(loader: &str) -> String {
    let trimmed = loader.trim().to_lowercase();
    if trimmed.is_empty() {
        "vanilla".to_string()
    } else {
        trimmed
    }
}

fn vanilla_enhanced_plan(
    family: VersionFamily,
    loader: String,
    mode: PerformanceMode,
) -> CompositionPlan {
    CompositionPlan {
        composition_id: String::new(),
        family,
        loader,
        mode,
        tier: CompositionTier::VanillaEnhanced,
        mods: Vec::new(),
        jvm_preset: String::new(),
        warnings: Vec::new(),
        fallback_reason: String::new(),
    }
}

fn matching_compositions<'a>(
    manifest: &'a Manifest,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> Vec<&'a CompositionDef> {
    manifest
        .compositions
        .iter()
        .filter(|definition| {
            if definition.tier != tier {
                return false;
            }
            if !definition.families.contains(&family) {
                return false;
            }
            if !definition
                .loaders
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(loader))
            {
                return false;
            }
            true
        })
        .collect()
}

fn active_composition_disable<'a>(
    manifest: &'a Manifest,
    definition: &CompositionDef,
    family: VersionFamily,
    loader: &str,
) -> Option<&'a EmergencyDisable> {
    manifest.emergency_disables.iter().find(|disable| {
        disable.target == EmergencyDisableTarget::Composition
            && disable.target_id == definition.id
            && disable_applies(disable, family, loader, definition.tier)
    })
}

fn active_artifact_disable<'a>(
    manifest: &'a Manifest,
    managed_mod: &ManagedMod,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> Option<&'a EmergencyDisable> {
    manifest.emergency_disables.iter().find(|disable| {
        disable.target == EmergencyDisableTarget::Artifact
            && artifact_target_matches(manifest, disable, managed_mod)
            && disable_applies(disable, family, loader, tier)
    })
}

fn artifact_target_matches(
    manifest: &Manifest,
    disable: &EmergencyDisable,
    managed_mod: &ManagedMod,
) -> bool {
    let Some(artifact) = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.id.eq_ignore_ascii_case(&managed_mod.artifact_id))
    else {
        return false;
    };
    disable.target_id.eq_ignore_ascii_case(&artifact.id)
        || disable
            .target_id
            .eq_ignore_ascii_case(&artifact.source.project_id)
}

fn disable_applies(
    disable: &EmergencyDisable,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> bool {
    (disable.families.is_empty() || disable.families.contains(&family))
        && (disable.loaders.is_empty()
            || disable
                .loaders
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(loader)))
        && (disable.tiers.is_empty() || disable.tiers.contains(&tier))
}

fn composition_disable_warning(disable: &EmergencyDisable, composition_id: &str) -> String {
    format!(
        "{composition_id} skipped by emergency disable {}: {}",
        disable.id, disable.reason
    )
}

fn artifact_disable_warning(disable: &EmergencyDisable, managed_mod: &ManagedMod) -> String {
    format!(
        "{} skipped by emergency disable {}: {}",
        managed_mod.slug, disable.id, disable.reason
    )
}

fn push_unique_warning(warnings: &mut Vec<String>, warning: String) {
    if !warnings.iter().any(|existing| existing == &warning) {
        warnings.push(warning);
    }
}

fn should_include_mod(
    managed_mod: &ManagedMod,
    game_version: &str,
    hardware: &HardwareProfile,
    installed: &std::collections::HashSet<String>,
) -> (bool, String) {
    if !managed_mod.exact_game_versions.is_empty()
        && !managed_mod
            .exact_game_versions
            .iter()
            .any(|candidate| candidate == game_version)
    {
        return (false, String::new());
    }

    if !managed_mod.version_range.trim().is_empty() {
        let Ok(version) = parse_version(game_version) else {
            return (false, String::new());
        };
        if !version_in_range(&version, &managed_mod.version_range) {
            return (false, String::new());
        }
    }

    if matches!(managed_mod.condition, ModCondition::Recommend) {
        return (false, String::new());
    }

    let (ok, warning) = satisfies_hardware(managed_mod, hardware);
    if !ok {
        return (false, warning);
    }

    for exclusion in &managed_mod.mutual_exclusions {
        if installed.contains(&exclusion.to_lowercase()) {
            return (
                false,
                format!(
                    "{} skipped: incompatible with managed mod {}",
                    managed_mod.slug, exclusion
                ),
            );
        }
    }

    (true, String::new())
}

fn satisfies_hardware(managed_mod: &ManagedMod, hardware: &HardwareProfile) -> (bool, String) {
    let Some(requirement) = &managed_mod.hardware_req else {
        return (true, String::new());
    };
    if !requirement.gpu_vendor.is_empty()
        && !hardware
            .gpu_vendor
            .eq_ignore_ascii_case(&requirement.gpu_vendor)
    {
        if requirement.gpu_vendor.eq_ignore_ascii_case("nvidia") {
            return (
                false,
                format!(
                    "{} skipped: no NVIDIA Turing+ GPU detected",
                    managed_mod.slug
                ),
            );
        }
        return (
            false,
            format!("{} skipped: unsupported GPU vendor", managed_mod.slug),
        );
    }
    if requirement.gpu_arch_min > 0 && hardware.gpu_arch < requirement.gpu_arch_min {
        return (
            false,
            format!(
                "{} skipped: no NVIDIA Turing+ GPU detected",
                managed_mod.slug
            ),
        );
    }
    if requirement.min_ram_mb > 0 && hardware.total_ram_mb < requirement.min_ram_mb {
        return (
            false,
            format!("{} skipped: not enough system RAM", managed_mod.slug),
        );
    }
    if requirement.min_cores > 0 && hardware.logical_cores < requirement.min_cores {
        return (
            false,
            format!("{} skipped: not enough CPU cores", managed_mod.slug),
        );
    }
    (true, String::new())
}
