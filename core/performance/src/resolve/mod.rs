use crate::types::{
    CompositionDef, CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
    HardwareProfile, ManagedMod, Manifest, ModCondition, PerformanceMode, ResolutionRequest,
    VersionFamily,
};
use regex::Regex;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::Command;
use std::sync::OnceLock;
#[cfg(target_os = "windows")]
use std::sync::mpsc;
#[cfg(target_os = "windows")]
use std::time::Duration;
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
    #[error("emergency disable id is required")]
    MissingEmergencyDisableId,
    #[error("emergency disable target_id is required")]
    MissingEmergencyDisableTargetId,
    #[error("emergency disable reason is required")]
    MissingEmergencyDisableReason,
    #[error("duplicate emergency disable id: {0}")]
    DuplicateEmergencyDisableId(String),
    #[error("emergency composition disable references unknown composition: {0}")]
    UnknownEmergencyDisableComposition(String),
    #[error("emergency artifact disable references unknown managed artifact: {0}")]
    UnknownEmergencyDisableArtifact(String),
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

    let mut artifact_ids = std::collections::HashSet::new();
    for composition in &manifest.compositions {
        for managed_mod in &composition.mods {
            if !managed_mod.project_id.is_empty() {
                artifact_ids.insert(managed_mod.project_id.to_lowercase());
            }
            if !managed_mod.slug.is_empty() {
                artifact_ids.insert(managed_mod.slug.to_lowercase());
            }
        }
    }

    let mut disable_ids = std::collections::HashSet::new();
    for disable in &manifest.emergency_disables {
        if disable.id.trim().is_empty() {
            return Err(ResolveError::MissingEmergencyDisableId);
        }
        if !disable_ids.insert(disable.id.clone()) {
            return Err(ResolveError::DuplicateEmergencyDisableId(
                disable.id.clone(),
            ));
        }
        if disable.target_id.trim().is_empty() {
            return Err(ResolveError::MissingEmergencyDisableTargetId);
        }
        if disable.reason.trim().is_empty() {
            return Err(ResolveError::MissingEmergencyDisableReason);
        }
        match disable.target {
            EmergencyDisableTarget::Composition => {
                if !ids.contains(&disable.target_id) {
                    return Err(ResolveError::UnknownEmergencyDisableComposition(
                        disable.target_id.clone(),
                    ));
                }
            }
            EmergencyDisableTarget::Artifact => {
                if !artifact_ids.contains(&disable.target_id.to_lowercase()) {
                    return Err(ResolveError::UnknownEmergencyDisableArtifact(
                        disable.target_id.clone(),
                    ));
                }
            }
        }
    }

    Ok(())
}

pub fn detect_hardware() -> HardwareProfile {
    static HARDWARE_PROFILE: OnceLock<HardwareProfile> = OnceLock::new();

    HARDWARE_PROFILE
        .get_or_init(detect_hardware_uncached)
        .clone()
}

fn detect_hardware_uncached() -> HardwareProfile {
    let mut system = System::new();
    system.refresh_memory();

    let total_ram_mb = (system.total_memory() / (1024 * 1024)).min(i32::MAX as u64) as i32;
    let logical_cores = std::thread::available_parallelism()
        .map(|value| value.get() as i32)
        .unwrap_or(1);
    let (gpu_vendor, gpu_arch) = detect_gpu();

    HardwareProfile {
        total_ram_mb,
        logical_cores,
        gpu_vendor,
        gpu_arch,
    }
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (String, i32) {
    let mut best_vendor = String::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return (best_vendor, 0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_drm_card_path(&path) {
            continue;
        }

        let Ok(vendor_id) = fs::read_to_string(path.join("device/vendor")) else {
            continue;
        };
        let Some(vendor) = gpu_vendor_from_pci_id(&vendor_id) else {
            continue;
        };

        if vendor == "nvidia" {
            return (vendor.to_string(), detect_nvidia_arch_linux());
        }
        if vendor == "amd" && best_vendor != "amd" {
            best_vendor = vendor.to_string();
        }
        if vendor == "intel" && best_vendor.is_empty() {
            best_vendor = vendor.to_string();
        }
    }

    (best_vendor, 0)
}

#[cfg(target_os = "linux")]
fn is_drm_card_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(number) = name.strip_prefix("card") else {
        return false;
    };
    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
}

#[cfg(target_os = "linux")]
fn detect_nvidia_arch_linux() -> i32 {
    let Ok(entries) = fs::read_dir("/proc/driver/nvidia/gpus") else {
        return 0;
    };

    entries
        .flatten()
        .filter_map(|entry| nvidia_model_from_information_file(entry.path().join("information")))
        .map(|model| nvidia_arch_from_model(&model))
        .find(|arch| *arch > 0)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn nvidia_model_from_information_file(path: PathBuf) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    nvidia_model_from_information(&contents)
}

#[cfg(target_os = "windows")]
fn detect_gpu() -> (String, i32) {
    let Some(output) = run_windows_gpu_query() else {
        return (String::new(), 0);
    };
    let names = parse_windows_gpu_names(&output);
    select_gpu_from_names(&names)
}

#[cfg(target_os = "windows")]
fn run_windows_gpu_query() -> Option<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let output = Command::new("wmic")
            .args(["path", "win32_VideoController", "get", "name"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    decode_windows_command_output(&output.stdout)
                } else {
                    None
                }
            })
            .or_else(|| {
                Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
                    ])
                    .output()
                    .ok()
                    .and_then(|output| {
                        if output.status.success() {
                            decode_windows_command_output(&output.stdout)
                        } else {
                            None
                        }
                    })
            });
        let _ = sender.send(output);
    });

    receiver.recv_timeout(Duration::from_secs(2)).ok().flatten()
}

#[cfg(target_os = "windows")]
fn decode_windows_command_output(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    let looks_utf16le = bytes.starts_with(&[0xff, 0xfe])
        || bytes
            .iter()
            .skip(1)
            .step_by(2)
            .take(32)
            .filter(|byte| **byte == 0)
            .count()
            >= 8;
    if looks_utf16le {
        let mut values = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.chunks_exact(2) {
            values.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        return String::from_utf16(&values)
            .ok()
            .map(|value| value.trim_start_matches('\u{feff}').to_string());
    }

    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg(target_os = "windows")]
fn parse_windows_gpu_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.eq_ignore_ascii_case("name"))
        .map(str::to_string)
        .collect()
}

#[cfg(target_os = "windows")]
fn select_gpu_from_names(names: &[String]) -> (String, i32) {
    for vendor in ["nvidia", "amd", "intel"] {
        if let Some(name) = names
            .iter()
            .find(|name| gpu_vendor_from_model_name(name) == Some(vendor))
        {
            let arch = if vendor == "nvidia" {
                nvidia_arch_from_model(name)
            } else {
                0
            };
            return (vendor.to_string(), arch);
        }
    }
    (String::new(), 0)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_gpu() -> (String, i32) {
    (String::new(), 0)
}

fn gpu_vendor_from_pci_id(value: &str) -> Option<&'static str> {
    match value.trim().to_lowercase().as_str() {
        "0x10de" => Some("nvidia"),
        "0x1002" | "0x1022" => Some("amd"),
        "0x8086" => Some("intel"),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn gpu_vendor_from_model_name(value: &str) -> Option<&'static str> {
    let normalized = value.to_lowercase();
    if normalized.contains("nvidia") || normalized.contains("geforce") || normalized.contains("rtx")
    {
        Some("nvidia")
    } else if normalized.contains("amd")
        || normalized.contains("radeon")
        || normalized.contains("advanced micro devices")
    {
        Some("amd")
    } else if normalized.contains("intel") {
        Some("intel")
    } else {
        None
    }
}

fn nvidia_model_from_information(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("model") {
            let model = value.trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
        None
    })
}

fn nvidia_arch_from_model(model: &str) -> i32 {
    let normalized = model.to_uppercase();
    for (needle, arch) in [
        ("RTX 50", 4),
        ("RTX 40", 4),
        ("RTX 30", 3),
        ("RTX 20", 2),
        ("GTX 16", 2),
        ("GTX 10", 1),
    ] {
        if normalized.contains(needle) {
            return arch;
        }
    }
    0
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

    let mut skipped_warnings = Vec::new();
    for tier in [
        CompositionTier::Extended,
        CompositionTier::Core,
        CompositionTier::VanillaEnhanced,
    ] {
        for definition in matching_compositions(manifest, family, &loader, tier) {
            if let Some(disable) = active_composition_disable(manifest, definition, family, &loader)
            {
                skipped_warnings.push(composition_disable_warning(disable, &definition.id));
                continue;
            }

            let mut active_mods = Vec::new();
            let mut warnings = skipped_warnings.clone();
            for managed_mod in &definition.mods {
                if let Some(disable) =
                    active_artifact_disable(manifest, managed_mod, family, &loader, definition.tier)
                {
                    warnings.push(artifact_disable_warning(disable, managed_mod));
                    continue;
                }

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
                    plan.fallback_reason = if skipped_warnings.is_empty() {
                        "higher-tier managed composition is unavailable for this combination"
                            .to_string()
                    } else {
                        "a higher-tier managed composition is temporarily disabled".to_string()
                    };
                }
                return plan;
            }
        }
    }

    let mut plan = vanilla_enhanced_plan(family, loader, mode);
    plan.warnings = skipped_warnings;
    if !plan.warnings.is_empty() {
        plan.fallback_reason = "managed compositions are temporarily disabled".to_string();
    }
    plan
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
            && artifact_target_matches(disable, managed_mod)
            && disable_applies(disable, family, loader, tier)
    })
}

fn artifact_target_matches(disable: &EmergencyDisable, managed_mod: &ManagedMod) -> bool {
    disable
        .target_id
        .eq_ignore_ascii_case(&managed_mod.project_id)
        || disable.target_id.eq_ignore_ascii_case(&managed_mod.slug)
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
    use super::{
        ResolutionRequest, ResolveError, builtin_manifest, gpu_vendor_from_pci_id,
        nvidia_arch_from_model, nvidia_model_from_information, parse_mode, resolve_plan,
        validate_manifest,
    };
    use crate::types::{
        CompositionTier, EmergencyDisable, EmergencyDisableTarget, HardwareProfile, Manifest,
        PerformanceMode, VersionFamily,
    };

    #[test]
    fn families_a_through_d_managed_plans_resolve_named_vanilla_enhanced_compositions() {
        let manifest = builtin_manifest().expect("manifest");

        for (game_version, family, composition_id) in [
            ("1.5.2", VersionFamily::A, "family-a-vanilla-enhanced"),
            ("1.7.10", VersionFamily::B, "family-b-vanilla-enhanced"),
            ("1.12.2", VersionFamily::C, "family-c-vanilla-enhanced"),
            ("1.15.2", VersionFamily::D, "family-d-vanilla-enhanced"),
        ] {
            for loader in ["vanilla", "fabric", "forge", "neoforge", "quilt"] {
                let plan = resolve_plan(
                    Some(&manifest),
                    ResolutionRequest {
                        game_version: game_version.to_string(),
                        loader: loader.to_string(),
                        mode: PerformanceMode::Managed,
                        hardware: HardwareProfile::default(),
                        installed_mods: Vec::new(),
                    },
                );

                assert_eq!(plan.composition_id, composition_id);
                assert_eq!(plan.family, family);
                assert_eq!(plan.loader, loader);
                assert_eq!(plan.tier, CompositionTier::VanillaEnhanced);
                assert!(plan.mods.is_empty());
            }
        }
    }

    #[test]
    fn fabric_family_e_and_f_managed_plans_resolve_real_mods() {
        let manifest = builtin_manifest().expect("manifest");

        for (game_version, expected_family) in
            [("1.20.1", VersionFamily::E), ("1.20.4", VersionFamily::F)]
        {
            let plan = resolve_plan(
                Some(&manifest),
                ResolutionRequest {
                    game_version: game_version.to_string(),
                    loader: "fabric".to_string(),
                    mode: PerformanceMode::Managed,
                    hardware: HardwareProfile::default(),
                    installed_mods: Vec::new(),
                },
            );

            assert_eq!(plan.family, expected_family);
            assert_eq!(plan.loader, "fabric");
            assert!(plan.composition_id.contains("fabric"));
            assert!(
                plan.mods
                    .iter()
                    .any(|managed_mod| managed_mod.slug == "sodium")
            );
        }
    }

    #[test]
    fn parse_mode_accepts_supported_values() {
        assert_eq!(parse_mode("managed"), Some(PerformanceMode::Managed));
        assert_eq!(parse_mode("vanilla"), Some(PerformanceMode::Vanilla));
        assert_eq!(parse_mode("custom"), Some(PerformanceMode::Custom));
        assert_eq!(parse_mode("invalid"), None);
    }

    #[test]
    fn pci_vendor_ids_map_to_gpu_vendors() {
        assert_eq!(gpu_vendor_from_pci_id("0x10de\n"), Some("nvidia"));
        assert_eq!(gpu_vendor_from_pci_id("0X1002"), Some("amd"));
        assert_eq!(gpu_vendor_from_pci_id("0x1022"), Some("amd"));
        assert_eq!(gpu_vendor_from_pci_id(" 0x8086 "), Some("intel"));
        assert_eq!(gpu_vendor_from_pci_id("0x1234"), None);
    }

    #[test]
    fn nvidia_model_strings_infer_arch_generation() {
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce GTX 1080"), 1);
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce GTX 1660 Ti"), 2);
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 2060"), 2);
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 3080"), 3);
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 4090"), 4);
        assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 5090"), 4);
        assert_eq!(nvidia_arch_from_model("NVIDIA Quadro P2000"), 0);
    }

    #[test]
    fn nvidia_proc_information_parser_reads_model_line() {
        assert_eq!(
            nvidia_model_from_information(
                "Model: \t\t NVIDIA GeForce RTX 3080\nIRQ: 54\nGPU UUID: GPU-test\n"
            )
            .as_deref(),
            Some("NVIDIA GeForce RTX 3080")
        );
        assert_eq!(nvidia_model_from_information("IRQ: 54\n"), None);
    }

    #[test]
    fn nvidium_requires_nvidia_turing_or_newer() {
        for (hardware, expected_included) in [
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 2,
                    ..HardwareProfile::default()
                },
                true,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 3,
                    ..HardwareProfile::default()
                },
                true,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 1,
                    ..HardwareProfile::default()
                },
                false,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 0,
                    ..HardwareProfile::default()
                },
                false,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "amd".to_string(),
                    gpu_arch: 0,
                    ..HardwareProfile::default()
                },
                false,
            ),
        ] {
            let plan = resolve_plan(
                Some(&builtin_manifest().expect("manifest")),
                ResolutionRequest {
                    game_version: "1.20.4".to_string(),
                    loader: "fabric".to_string(),
                    mode: PerformanceMode::Managed,
                    hardware,
                    installed_mods: Vec::new(),
                },
            );

            assert_eq!(
                plan.mods
                    .iter()
                    .any(|managed_mod| managed_mod.slug == "nvidium"),
                expected_included
            );
            assert_eq!(
                plan.warnings
                    .iter()
                    .any(|warning| warning == "nvidium skipped: no NVIDIA Turing+ GPU detected"),
                !expected_included
            );
        }
    }

    #[test]
    fn manifest_without_emergency_disables_is_not_current_schema() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "compositions": []
        }))
        .expect_err("missing emergency_disables should be invalid current schema");

        assert!(error.to_string().contains("emergency_disables"));
    }

    #[test]
    fn validation_rejects_invalid_emergency_disables() {
        for (disable, expected) in [
            (
                EmergencyDisable {
                    id: String::new(),
                    target: EmergencyDisableTarget::Composition,
                    target_id: "family-f-fabric-extended".to_string(),
                    reason: "Temporary hold.".to_string(),
                    families: Vec::new(),
                    loaders: Vec::new(),
                    tiers: Vec::new(),
                },
                ResolveError::MissingEmergencyDisableId,
            ),
            (
                EmergencyDisable {
                    id: "missing-target".to_string(),
                    target: EmergencyDisableTarget::Composition,
                    target_id: String::new(),
                    reason: "Temporary hold.".to_string(),
                    families: Vec::new(),
                    loaders: Vec::new(),
                    tiers: Vec::new(),
                },
                ResolveError::MissingEmergencyDisableTargetId,
            ),
            (
                EmergencyDisable {
                    id: "missing-reason".to_string(),
                    target: EmergencyDisableTarget::Composition,
                    target_id: "family-f-fabric-extended".to_string(),
                    reason: String::new(),
                    families: Vec::new(),
                    loaders: Vec::new(),
                    tiers: Vec::new(),
                },
                ResolveError::MissingEmergencyDisableReason,
            ),
        ] {
            let mut manifest = builtin_manifest().expect("manifest");
            manifest.emergency_disables.push(disable);

            assert_error_kind(validate_manifest(&manifest), expected);
        }
    }

    #[test]
    fn validation_rejects_duplicate_emergency_disable_ids() {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.emergency_disables.push(test_composition_disable(
            "duplicate",
            "family-f-fabric-extended",
        ));
        manifest
            .emergency_disables
            .push(test_artifact_disable("duplicate", "sodium"));

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::DuplicateEmergencyDisableId("duplicate".to_string()),
        );
    }

    #[test]
    fn validation_rejects_unknown_emergency_disable_targets() {
        let mut composition_manifest = builtin_manifest().expect("manifest");
        composition_manifest
            .emergency_disables
            .push(test_composition_disable(
                "unknown-composition",
                "missing-composition",
            ));
        assert_error_kind(
            validate_manifest(&composition_manifest),
            ResolveError::UnknownEmergencyDisableComposition("missing-composition".to_string()),
        );

        let mut artifact_manifest = builtin_manifest().expect("manifest");
        artifact_manifest
            .emergency_disables
            .push(test_artifact_disable(
                "unknown-artifact",
                "missing-artifact",
            ));
        assert_error_kind(
            validate_manifest(&artifact_manifest),
            ResolveError::UnknownEmergencyDisableArtifact("missing-artifact".to_string()),
        );
    }

    #[test]
    fn disabled_composition_falls_back_to_next_eligible_tier() {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.emergency_disables.push(test_composition_disable(
            "hold-family-f-extended",
            "family-f-fabric-extended",
        ));

        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.20.4".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.composition_id, "family-f-fabric-core");
        assert_eq!(plan.tier, CompositionTier::Core);
        assert_eq!(
            plan.fallback_reason,
            "a higher-tier managed composition is temporarily disabled"
        );
        assert!(plan.warnings.iter().any(|warning| {
            warning.contains("family-f-fabric-extended skipped by emergency disable")
                && warning.contains("Temporary hold.")
        }));
    }

    #[test]
    fn artifact_disable_drops_matching_managed_mod_with_warning() {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest
            .emergency_disables
            .push(test_artifact_disable("hold-sodium", "sodium"));

        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.20.4".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.composition_id, "family-f-fabric-extended");
        assert!(
            plan.mods
                .iter()
                .all(|managed_mod| managed_mod.slug != "sodium")
        );
        assert!(plan.warnings.iter().any(|warning| {
            warning.contains("sodium skipped by emergency disable hold-sodium")
                && warning.contains("Temporary hold.")
        }));
    }

    fn test_composition_disable(id: &str, target_id: &str) -> EmergencyDisable {
        EmergencyDisable {
            id: id.to_string(),
            target: EmergencyDisableTarget::Composition,
            target_id: target_id.to_string(),
            reason: "Temporary hold.".to_string(),
            families: Vec::new(),
            loaders: Vec::new(),
            tiers: Vec::new(),
        }
    }

    fn test_artifact_disable(id: &str, target_id: &str) -> EmergencyDisable {
        EmergencyDisable {
            id: id.to_string(),
            target: EmergencyDisableTarget::Artifact,
            target_id: target_id.to_string(),
            reason: "Temporary hold.".to_string(),
            families: Vec::new(),
            loaders: Vec::new(),
            tiers: Vec::new(),
        }
    }

    fn assert_error_kind(result: Result<(), ResolveError>, expected: ResolveError) {
        let error = result.expect_err("manifest should be invalid");
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected)
        );
        assert_eq!(error.to_string(), expected.to_string());
    }
}
