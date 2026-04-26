use crate::types::{
    CompositionDef, CompositionPlan, CompositionTier, HardwareProfile, ManagedMod, Manifest,
    ModCondition, PerformanceMode, ResolutionRequest, VersionFamily,
};
use regex::Regex;
use std::sync::OnceLock;
use sysinfo::System;
use thiserror::Error;

const BUILTIN_CATALOG: &str = include_str!("../catalog.json");

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("failed to parse builtin performance manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported schema_version")]
    UnsupportedSchema,
    #[error("composition id is required")]
    MissingCompositionId,
    #[error("duplicate composition id: {0}")]
    DuplicateCompositionId(String),
    #[error("fallback_to references unknown composition: {0}")]
    UnknownFallback(String),
}

pub fn builtin_manifest() -> Result<Manifest, ResolveError> {
    let manifest = serde_json::from_str::<Manifest>(BUILTIN_CATALOG)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), ResolveError> {
    if manifest.schema_version != 1 {
        return Err(ResolveError::UnsupportedSchema);
    }

    let mut ids = std::collections::HashSet::new();
    for composition in &manifest.compositions {
        if composition.id.is_empty() {
            return Err(ResolveError::MissingCompositionId);
        }
        if !ids.insert(composition.id.clone()) {
            return Err(ResolveError::DuplicateCompositionId(composition.id.clone()));
        }
    }

    for composition in &manifest.compositions {
        if !composition.fallback_to.is_empty() && !ids.contains(&composition.fallback_to) {
            return Err(ResolveError::UnknownFallback(
                composition.fallback_to.clone(),
            ));
        }
    }

    Ok(())
}

pub fn detect_hardware() -> HardwareProfile {
    let mut system = System::new();
    system.refresh_memory();

    let total_ram_mb = (system.total_memory() / (1024 * 1024)).min(i32::MAX as u64) as i32;
    let logical_cores = std::thread::available_parallelism()
        .map(|value| value.get() as i32)
        .unwrap_or(1);

    HardwareProfile {
        total_ram_mb,
        logical_cores,
        gpu_vendor: String::new(),
        gpu_arch: 0,
    }
}

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
            fallback_chain: Vec::new(),
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

    for tier in [
        CompositionTier::Extended,
        CompositionTier::Core,
        CompositionTier::VanillaEnhanced,
    ] {
        let Some(definition) = find_composition(manifest, family, &loader, tier) else {
            continue;
        };

        let mut active_mods = Vec::new();
        let mut warnings = Vec::new();
        for managed_mod in &definition.mods {
            let (include, warning) = should_include_mod(
                managed_mod,
                &request.game_version,
                &request.hardware,
                &installed_set,
            );
            if !warning.is_empty() {
                warnings.push(warning);
            }
            if include {
                active_mods.push(managed_mod.clone());
            }
        }

        if active_mods.len() >= 2 || matches!(tier, CompositionTier::VanillaEnhanced) {
            let mut plan = CompositionPlan {
                composition_id: definition.id.clone(),
                family,
                loader: loader.clone(),
                mode,
                tier: definition.tier,
                mods: active_mods,
                jvm_preset: definition.jvm_preset.clone(),
                fallback_chain: fallback_chain(manifest, &definition.id),
                warnings,
                fallback_reason: String::new(),
            };
            if !matches!(tier, CompositionTier::Extended) {
                plan.fallback_reason =
                    "higher-tier managed composition is unavailable for this combination"
                        .to_string();
            }
            return plan;
        }
    }

    vanilla_enhanced_plan(family, loader, mode)
}

pub fn extract_base_version(version_id: &str) -> String {
    let mut fallback = String::new();
    for part in version_id
        .split('-')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if parse_version(part).is_ok() {
            return part.to_string();
        }
        if fallback.is_empty() && part.matches('.').count() >= 1 {
            fallback = part.to_string();
        }
    }
    if fallback.is_empty() {
        version_id.to_string()
    } else {
        fallback
    }
}

pub fn infer_loader_from_version_id(version_id: &str) -> String {
    let value = version_id.to_lowercase();
    if value.contains("neoforge") {
        "neoforge".to_string()
    } else if value.contains("fabric") {
        "fabric".to_string()
    } else if value.contains("forge") {
        "forge".to_string()
    } else if value.contains("quilt") {
        "quilt".to_string()
    } else {
        "vanilla".to_string()
    }
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
        fallback_chain: Vec::new(),
        warnings: Vec::new(),
        fallback_reason: String::new(),
    }
}

fn find_composition(
    manifest: &Manifest,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> Option<CompositionDef> {
    manifest.compositions.iter().find_map(|definition| {
        if definition.tier != tier {
            return None;
        }
        if !definition.families.contains(&family) {
            return None;
        }
        if !definition
            .loaders
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(loader))
        {
            return None;
        }
        Some(definition.clone())
    })
}

fn fallback_chain(manifest: &Manifest, start_id: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = start_id.to_string();

    while !current.is_empty() && seen.insert(current.clone()) {
        let Some(definition) = manifest
            .compositions
            .iter()
            .find(|definition| definition.id == current)
        else {
            break;
        };
        if definition.fallback_to.is_empty() {
            break;
        }
        chain.push(definition.fallback_to.clone());
        current = definition.fallback_to.clone();
    }

    chain
}

fn should_include_mod(
    managed_mod: &ManagedMod,
    game_version: &str,
    hardware: &HardwareProfile,
    installed: &std::collections::HashSet<String>,
) -> (bool, String) {
    match managed_mod.condition {
        ModCondition::Always => {}
        ModCondition::VersionRange => {
            let Ok(version) = parse_version(game_version) else {
                return (false, String::new());
            };
            if !version_in_range(&version, &managed_mod.version_range) {
                return (false, String::new());
            }
        }
        ModCondition::Hardware => {
            let (ok, warning) = satisfies_hardware(managed_mod, hardware);
            if !ok {
                return (false, warning);
            }
        }
        ModCondition::Recommend => {
            return (false, String::new());
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct MCVersion {
    major: i32,
    minor: i32,
    patch: i32,
    is_snapshot: bool,
    raw: String,
}

fn parse_version(value: &str) -> Result<MCVersion, ()> {
    static RELEASE_PATTERN: OnceLock<Regex> = OnceLock::new();
    static SNAPSHOT_PATTERN: OnceLock<Regex> = OnceLock::new();

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(());
    }

    let snapshot = SNAPSHOT_PATTERN
        .get_or_init(|| Regex::new(r"^\d+w\d+[a-z]$").expect("snapshot regex"))
        .is_match(&trimmed.to_lowercase());
    if snapshot {
        return Ok(MCVersion {
            major: 0,
            minor: 0,
            patch: 0,
            is_snapshot: true,
            raw: trimmed.to_string(),
        });
    }

    let captures = RELEASE_PATTERN
        .get_or_init(|| Regex::new(r"^(\d+)\.(\d+)(?:\.(\d+))?$").expect("release regex"))
        .captures(trimmed)
        .ok_or(())?;

    Ok(MCVersion {
        major: captures
            .get(1)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .ok_or(())?,
        minor: captures
            .get(2)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .ok_or(())?,
        patch: captures
            .get(3)
            .and_then(|value| value.as_str().parse::<i32>().ok())
            .unwrap_or(0),
        is_snapshot: false,
        raw: trimmed.to_string(),
    })
}

fn compare_release_version(version: &MCVersion, major: i32, minor: i32, patch: i32) -> i32 {
    compare_versions(
        version,
        &MCVersion {
            major,
            minor,
            patch,
            is_snapshot: false,
            raw: String::new(),
        },
    )
}

fn compare_versions(left: &MCVersion, right: &MCVersion) -> i32 {
    if left.is_snapshot && !right.is_snapshot {
        return 1;
    }
    if !left.is_snapshot && right.is_snapshot {
        return -1;
    }
    if left.is_snapshot && right.is_snapshot {
        return match left.raw.to_lowercase().cmp(&right.raw.to_lowercase()) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    for ordering in [
        left.major.cmp(&right.major),
        left.minor.cmp(&right.minor),
        left.patch.cmp(&right.patch),
    ] {
        if ordering.is_lt() {
            return -1;
        }
        if ordering.is_gt() {
            return 1;
        }
    }
    0
}

fn version_in_range(version: &MCVersion, range: &str) -> bool {
    let trimmed = range.trim();
    if trimmed.is_empty() {
        return true;
    }
    for condition in trimmed.split_whitespace() {
        let (operator, raw_target) = split_range_condition(condition);
        let Ok(target) = parse_version(raw_target) else {
            return false;
        };
        let compare = compare_versions(version, &target);
        let matches = match operator {
            ">" => compare > 0,
            ">=" => compare >= 0,
            "<" => compare < 0,
            "<=" => compare <= 0,
            "=" => compare == 0,
            _ => false,
        };
        if !matches {
            return false;
        }
    }
    true
}

fn split_range_condition(condition: &str) -> (&str, &str) {
    for operator in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = condition.strip_prefix(operator) {
            return (operator, rest.trim());
        }
    }
    ("=", condition)
}

#[cfg(test)]
mod tests {
    use super::{ResolutionRequest, builtin_manifest, detect_hardware, parse_mode, resolve_plan};
    use crate::types::PerformanceMode;

    #[test]
    fn fabric_family_f_managed_plan_resolves_real_mods() {
        let manifest = builtin_manifest().expect("manifest");
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.20.4".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: detect_hardware(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.loader, "fabric");
        assert!(!plan.mods.is_empty());
    }

    #[test]
    fn parse_mode_accepts_supported_values() {
        assert_eq!(parse_mode("managed"), Some(PerformanceMode::Managed));
        assert_eq!(parse_mode("vanilla"), Some(PerformanceMode::Vanilla));
        assert_eq!(parse_mode("custom"), Some(PerformanceMode::Custom));
        assert_eq!(parse_mode("invalid"), None);
    }
}
