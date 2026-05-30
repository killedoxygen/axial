use crate::types::{
    CompositionDef, CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
    HardwareProfile, ManagedArtifactDefinition, ManagedMod, Manifest, ModCondition, OwnershipClass,
    PerformanceMode, ResolutionRequest, VersionFamily,
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
const RUNNING_APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const KNOWN_RULE_CHANNELS: &[&str] = &["bundled", "local", "remote"];

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("failed to parse builtin performance manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported schema_version")]
    UnsupportedSchema,
    #[error("minimum_app_version is required")]
    MissingMinimumAppVersion,
    #[error("minimum_app_version is invalid: {0}")]
    InvalidMinimumAppVersion(String),
    #[error("manifest requires app version {required}, but running app version is {running}")]
    UnsupportedAppVersion { required: String, running: String },
    #[error("rule_channel is required")]
    MissingRuleChannel,
    #[error("unsupported rule_channel: {0}")]
    UnsupportedRuleChannel(String),
    #[error("artifact id is required")]
    MissingArtifactId,
    #[error("duplicate artifact id: {0}")]
    DuplicateArtifactId(String),
    #[error("artifact {0} source project_id is required")]
    MissingArtifactProjectId(String),
    #[error("artifact {0} source slug is required")]
    MissingArtifactSlug(String),
    #[error("artifact {0} must be composition_managed")]
    InvalidArtifactOwnership(String),
    #[error("managed mod artifact_id is required")]
    MissingManagedModArtifactId,
    #[error("managed mod references unknown artifact: {0}")]
    UnknownManagedModArtifact(String),
    #[error("managed mod {artifact_id} project_id mismatch: expected {expected}, found {actual}")]
    ManagedModProjectMismatch {
        artifact_id: String,
        expected: String,
        actual: String,
    },
    #[error("managed mod {artifact_id} slug mismatch: expected {expected}, found {actual}")]
    ManagedModSlugMismatch {
        artifact_id: String,
        expected: String,
        actual: String,
    },
    #[error("managed mod {artifact_id} has invalid version_range: {version_range}")]
    InvalidManagedModVersionRange {
        artifact_id: String,
        version_range: String,
    },
    #[error("managed mod {artifact_id} has invalid hardware_req.{field}: {value}")]
    InvalidManagedModHardwareRequirement {
        artifact_id: String,
        field: &'static str,
        value: i32,
    },
    #[error("managed mod {artifact_id} has invalid mutual_exclusions.{field}: {value}")]
    InvalidManagedModMutualExclusion {
        artifact_id: String,
        field: &'static str,
        value: String,
    },
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
    validate_app_version_compatibility(&manifest.minimum_app_version)?;
    validate_rule_channel(&manifest.rule_channel)?;
    let artifacts = validate_artifacts(&manifest.artifacts)?;

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
        for managed_mod in &composition.mods {
            validate_managed_mod_artifact(managed_mod, &artifacts)?;
        }
    }

    let artifact_targets = declared_artifact_targets(&manifest.artifacts);

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
                if !artifact_targets.contains(&disable.target_id.to_lowercase()) {
                    return Err(ResolveError::UnknownEmergencyDisableArtifact(
                        disable.target_id.clone(),
                    ));
                }
            }
        }
    }

    Ok(())
}

fn validate_artifacts(
    artifacts: &[ManagedArtifactDefinition],
) -> Result<std::collections::HashMap<String, &ManagedArtifactDefinition>, ResolveError> {
    let mut ids = std::collections::HashSet::new();
    let mut by_id = std::collections::HashMap::new();
    for artifact in artifacts {
        if artifact.id.trim().is_empty() {
            return Err(ResolveError::MissingArtifactId);
        }
        let normalized_id = artifact.id.to_lowercase();
        if !ids.insert(normalized_id.clone()) {
            return Err(ResolveError::DuplicateArtifactId(artifact.id.clone()));
        }
        if artifact.source.project_id.trim().is_empty() {
            return Err(ResolveError::MissingArtifactProjectId(artifact.id.clone()));
        }
        if artifact.source.slug.trim().is_empty() {
            return Err(ResolveError::MissingArtifactSlug(artifact.id.clone()));
        }
        if artifact.ownership_class != OwnershipClass::CompositionManaged {
            return Err(ResolveError::InvalidArtifactOwnership(artifact.id.clone()));
        }
        by_id.insert(normalized_id, artifact);
    }
    Ok(by_id)
}

fn validate_managed_mod_artifact(
    managed_mod: &ManagedMod,
    artifacts: &std::collections::HashMap<String, &ManagedArtifactDefinition>,
) -> Result<(), ResolveError> {
    if managed_mod.artifact_id.trim().is_empty() {
        return Err(ResolveError::MissingManagedModArtifactId);
    }
    let Some(artifact) = artifacts.get(&managed_mod.artifact_id.to_lowercase()) else {
        return Err(ResolveError::UnknownManagedModArtifact(
            managed_mod.artifact_id.clone(),
        ));
    };
    if managed_mod.project_id != artifact.source.project_id {
        return Err(ResolveError::ManagedModProjectMismatch {
            artifact_id: managed_mod.artifact_id.clone(),
            expected: artifact.source.project_id.clone(),
            actual: managed_mod.project_id.clone(),
        });
    }
    if managed_mod.slug != artifact.source.slug {
        return Err(ResolveError::ManagedModSlugMismatch {
            artifact_id: managed_mod.artifact_id.clone(),
            expected: artifact.source.slug.clone(),
            actual: managed_mod.slug.clone(),
        });
    }
    validate_managed_mod_version_range(managed_mod)?;
    validate_managed_mod_hardware_req(managed_mod)?;
    validate_managed_mod_mutual_exclusions(managed_mod)?;
    Ok(())
}

fn validate_managed_mod_version_range(managed_mod: &ManagedMod) -> Result<(), ResolveError> {
    let trimmed = managed_mod.version_range.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    for condition in trimmed.split_whitespace() {
        let (_, raw_target) = split_range_condition(condition);
        if parse_version(raw_target).is_err() {
            return Err(ResolveError::InvalidManagedModVersionRange {
                artifact_id: managed_mod.artifact_id.clone(),
                version_range: trimmed.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_managed_mod_hardware_req(managed_mod: &ManagedMod) -> Result<(), ResolveError> {
    let Some(requirement) = &managed_mod.hardware_req else {
        return Ok(());
    };
    for (field, value) in [
        ("gpu_arch_min", requirement.gpu_arch_min),
        ("min_ram_mb", requirement.min_ram_mb),
        ("min_cores", requirement.min_cores),
    ] {
        if value < 0 {
            return Err(ResolveError::InvalidManagedModHardwareRequirement {
                artifact_id: managed_mod.artifact_id.clone(),
                field,
                value,
            });
        }
    }
    Ok(())
}

fn validate_managed_mod_mutual_exclusions(managed_mod: &ManagedMod) -> Result<(), ResolveError> {
    let mut exclusions = std::collections::HashSet::new();
    for exclusion in &managed_mod.mutual_exclusions {
        let trimmed = exclusion.trim();
        if trimmed.is_empty() || trimmed != exclusion {
            return Err(ResolveError::InvalidManagedModMutualExclusion {
                artifact_id: managed_mod.artifact_id.clone(),
                field: "entry",
                value: exclusion.clone(),
            });
        }
        if !exclusions.insert(exclusion.to_lowercase()) {
            return Err(ResolveError::InvalidManagedModMutualExclusion {
                artifact_id: managed_mod.artifact_id.clone(),
                field: "duplicate",
                value: exclusion.clone(),
            });
        }
    }
    Ok(())
}

fn declared_artifact_targets(
    artifacts: &[ManagedArtifactDefinition],
) -> std::collections::HashSet<String> {
    let mut targets = std::collections::HashSet::new();
    for artifact in artifacts {
        targets.insert(artifact.id.to_lowercase());
        targets.insert(artifact.source.project_id.to_lowercase());
        targets.insert(artifact.source.slug.to_lowercase());
    }
    targets
}

fn validate_app_version_compatibility(minimum_app_version: &str) -> Result<(), ResolveError> {
    let minimum = parse_app_version(minimum_app_version)?;
    let running = parse_app_version(RUNNING_APP_VERSION)
        .expect("CARGO_PKG_VERSION should be a numeric dotted app version");
    if compare_app_versions(&minimum, &running).is_gt() {
        return Err(ResolveError::UnsupportedAppVersion {
            required: minimum_app_version.trim().to_string(),
            running: RUNNING_APP_VERSION.to_string(),
        });
    }
    Ok(())
}

fn validate_rule_channel(rule_channel: &str) -> Result<(), ResolveError> {
    if rule_channel.trim().is_empty() {
        return Err(ResolveError::MissingRuleChannel);
    }
    if !KNOWN_RULE_CHANNELS.contains(&rule_channel) {
        return Err(ResolveError::UnsupportedRuleChannel(
            rule_channel.to_string(),
        ));
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
        || disable
            .target_id
            .eq_ignore_ascii_case(&artifact.source.slug)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre_release: Option<String>,
}

fn parse_app_version(value: &str) -> Result<AppVersion, ResolveError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ResolveError::MissingMinimumAppVersion);
    }

    let (release, pre_release) = match trimmed.split_once('-') {
        Some((release, pre_release)) => {
            if release.is_empty() || !valid_pre_release(pre_release) {
                return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
            }
            (release, Some(pre_release.to_string()))
        }
        None => (trimmed, None),
    };
    let parts = release.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
    }

    let parse_part = |part: &str| -> Result<u64, ResolveError> {
        if part.is_empty() || !part.chars().all(|character| character.is_ascii_digit()) {
            return Err(ResolveError::InvalidMinimumAppVersion(trimmed.to_string()));
        }
        part.parse::<u64>()
            .map_err(|_| ResolveError::InvalidMinimumAppVersion(trimmed.to_string()))
    };

    Ok(AppVersion {
        major: parse_part(parts[0])?,
        minor: parse_part(parts[1])?,
        patch: parse_part(parts[2])?,
        pre_release,
    })
}

fn valid_pre_release(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

fn compare_app_versions(left: &AppVersion, right: &AppVersion) -> std::cmp::Ordering {
    for ordering in [
        left.major.cmp(&right.major),
        left.minor.cmp(&right.minor),
        left.patch.cmp(&right.patch),
    ] {
        if !ordering.is_eq() {
            return ordering;
        }
    }
    match (&left.pre_release, &right.pre_release) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    }
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
        CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
        HardwareProfile, HardwareRequirement, ManagedMod, Manifest, OwnershipClass,
        PerformanceMode, VersionFamily,
    };

    const FAMILY_F_FABRIC_CORE_ADDITIONS: &[&str] = &[
        "scalablelux",
        "particle-core",
        "threadtweak",
        "badoptimizations",
    ];

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

        for (game_version, expected_family, expects_threadtweak) in [
            ("1.20.1", VersionFamily::E, false),
            ("1.20.4", VersionFamily::F, true),
        ] {
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
            assert_eq!(
                count_mods_with_slug(&plan.mods, "threadtweak"),
                usize::from(expects_threadtweak)
            );
        }
    }

    #[test]
    fn forge_family_e_and_f_resolve_extended_then_core_when_extended_disabled() {
        for (
            game_version,
            expected_family,
            extended_composition_id,
            core_composition_id,
            vanilla_composition_id,
        ) in [
            (
                "1.20.1",
                VersionFamily::E,
                "family-e-forge-extended",
                "family-e-forge-core",
                "family-e-vanilla-enhanced",
            ),
            (
                "1.20.4",
                VersionFamily::F,
                "family-f-forge-extended",
                "family-f-forge-core",
                "family-f-vanilla-enhanced",
            ),
        ] {
            for loader in ["forge", "neoforge"] {
                let manifest = builtin_manifest().expect("manifest");
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

                assert_eq!(plan.composition_id, extended_composition_id);
                assert_eq!(plan.family, expected_family);
                assert_eq!(plan.loader, loader);
                assert_eq!(plan.tier, CompositionTier::Extended);
                assert_eq!(
                    plan.fallback_chain,
                    vec![
                        core_composition_id.to_string(),
                        vanilla_composition_id.to_string()
                    ]
                );
                assert_eq!(count_mods_with_slug(&plan.mods, "embeddium"), 1);
                assert_eq!(count_mods_with_slug(&plan.mods, "ferrite-core"), 1);

                let mut disabled_manifest = builtin_manifest().expect("manifest");
                disabled_manifest
                    .emergency_disables
                    .push(test_composition_disable(
                        "hold-forge-extended",
                        extended_composition_id,
                    ));

                let fallback_plan = resolve_plan(
                    Some(&disabled_manifest),
                    ResolutionRequest {
                        game_version: game_version.to_string(),
                        loader: loader.to_string(),
                        mode: PerformanceMode::Managed,
                        hardware: HardwareProfile::default(),
                        installed_mods: Vec::new(),
                    },
                );

                assert_eq!(fallback_plan.composition_id, core_composition_id);
                assert_eq!(fallback_plan.family, expected_family);
                assert_eq!(fallback_plan.loader, loader);
                assert_eq!(fallback_plan.tier, CompositionTier::Core);
                assert_eq!(
                    fallback_plan.fallback_chain,
                    vec![vanilla_composition_id.to_string()]
                );
                assert_eq!(count_mods_with_slug(&fallback_plan.mods, "embeddium"), 1);
                assert_eq!(count_mods_with_slug(&fallback_plan.mods, "ferrite-core"), 1);
                assert_eq!(
                    fallback_plan.fallback_reason,
                    "a higher-tier managed composition is temporarily disabled"
                );
                let expected_warning =
                    format!("{extended_composition_id} skipped by emergency disable");
                assert!(fallback_plan.warnings.iter().any(|warning| {
                    warning.contains(&expected_warning) && warning.contains("Temporary hold.")
                }));
            }
        }
    }

    #[test]
    fn family_e_fabric_fallback_does_not_include_family_f_core_additions() {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.emergency_disables.push(test_composition_disable(
            "hold-family-e-extended",
            "family-e-fabric-extended",
        ));

        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.20.1".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.composition_id, "family-e-fabric-core");
        assert_eq!(plan.family, VersionFamily::E);
        assert_eq!(plan.tier, CompositionTier::Core);
        for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
            assert_eq!(count_mods_with_slug(&plan.mods, slug), 0, "{slug}");
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
    fn family_e_fabric_1_16_5_uses_older_version_gated_mods_without_nvidium() {
        let plan = fabric_plan("1.16.5", nvidia_turing_hardware());

        assert_eq!(plan.composition_id, "family-e-fabric-extended");
        assert_eq!(plan.family, VersionFamily::E);
        assert_eq!(plan.tier, CompositionTier::Extended);
        for slug in ["lazydfu", "smooth-boot-reloaded", "starlight"] {
            assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
        }
        assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 0);
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning == "nvidium skipped: no NVIDIA Turing+ GPU detected")
        );
    }

    #[test]
    fn family_e_fabric_1_20_1_uses_nvidium_without_older_version_gated_mods() {
        let plan = fabric_plan("1.20.1", nvidia_turing_hardware());

        assert_eq!(plan.composition_id, "family-e-fabric-extended");
        assert_eq!(plan.family, VersionFamily::E);
        assert_eq!(plan.tier, CompositionTier::Extended);
        assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 1);
        for slug in ["lazydfu", "smooth-boot-reloaded", "starlight"] {
            assert_eq!(count_mods_with_slug(&plan.mods, slug), 0, "{slug}");
        }
    }

    #[test]
    fn family_f_fabric_1_20_4_uses_nvidium_and_family_f_additions() {
        let plan = fabric_plan("1.20.4", nvidia_turing_hardware());

        assert_eq!(plan.composition_id, "family-f-fabric-extended");
        assert_eq!(plan.family, VersionFamily::F);
        assert_eq!(plan.tier, CompositionTier::Extended);
        assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 1);
        for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
            assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
        }
    }

    #[test]
    fn nvidium_requires_nvidia_turing_or_newer_for_applicable_versions() {
        for game_version in ["1.20.1", "1.20.4"] {
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
                let plan = fabric_plan(game_version, hardware);

                assert_eq!(
                    plan.mods
                        .iter()
                        .any(|managed_mod| managed_mod.slug == "nvidium"),
                    expected_included,
                    "{game_version}"
                );
                assert_eq!(
                    plan.warnings.iter().any(
                        |warning| warning == "nvidium skipped: no NVIDIA Turing+ GPU detected"
                    ),
                    !expected_included,
                    "{game_version}"
                );
            }
        }
    }

    #[test]
    fn nvidium_is_skipped_when_iris_is_installed() {
        let manifest = builtin_manifest().expect("manifest");
        let hardware = HardwareProfile {
            gpu_vendor: "nvidia".to_string(),
            gpu_arch: 2,
            ..HardwareProfile::default()
        };

        for game_version in ["1.20.1", "1.20.4"] {
            let plan = resolve_plan(
                Some(&manifest),
                ResolutionRequest {
                    game_version: game_version.to_string(),
                    loader: "fabric".to_string(),
                    mode: PerformanceMode::Managed,
                    hardware: hardware.clone(),
                    installed_mods: vec!["iris".to_string()],
                },
            );

            assert_eq!(plan.tier, CompositionTier::Extended);
            assert!(
                plan.mods
                    .iter()
                    .all(|managed_mod| managed_mod.slug != "nvidium")
            );
            assert!(plan.warnings.iter().any(|warning| {
                warning == "nvidium skipped: incompatible with managed mod iris"
            }));
        }
    }

    #[test]
    fn manifest_without_emergency_disables_is_not_current_schema() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "minimum_app_version": "0.3.1",
            "rule_channel": "bundled",
            "artifacts": [],
            "compositions": []
        }))
        .expect_err("missing emergency_disables should be invalid current schema");

        assert!(error.to_string().contains("emergency_disables"));
    }

    #[test]
    fn manifest_without_artifacts_is_not_current_schema() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "minimum_app_version": "0.3.1",
            "rule_channel": "bundled",
            "compositions": [],
            "emergency_disables": []
        }))
        .expect_err("missing artifacts should be invalid current schema");

        assert!(error.to_string().contains("artifacts"));
    }

    #[test]
    fn manifest_without_minimum_app_version_is_not_current_schema() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "rule_channel": "bundled",
            "artifacts": [],
            "compositions": [],
            "emergency_disables": []
        }))
        .expect_err("missing minimum_app_version should be invalid current schema");

        assert!(error.to_string().contains("minimum_app_version"));
    }

    #[test]
    fn manifest_without_rule_channel_is_not_current_schema() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "minimum_app_version": "0.3.1",
            "artifacts": [],
            "compositions": [],
            "emergency_disables": []
        }))
        .expect_err("missing rule_channel should be invalid current schema");

        assert!(error.to_string().contains("rule_channel"));
    }

    #[test]
    fn validation_rejects_incompatible_or_invalid_manifest_metadata() {
        let mut too_new = builtin_manifest().expect("manifest");
        too_new.minimum_app_version = "0.3.2".to_string();
        assert_error_kind(
            validate_manifest(&too_new),
            ResolveError::UnsupportedAppVersion {
                required: "0.3.2".to_string(),
                running: env!("CARGO_PKG_VERSION").to_string(),
            },
        );

        let mut invalid_version = builtin_manifest().expect("manifest");
        invalid_version.minimum_app_version = "latest".to_string();
        assert_error_kind(
            validate_manifest(&invalid_version),
            ResolveError::InvalidMinimumAppVersion("latest".to_string()),
        );

        let mut missing_version = builtin_manifest().expect("manifest");
        missing_version.minimum_app_version = String::new();
        assert_error_kind(
            validate_manifest(&missing_version),
            ResolveError::MissingMinimumAppVersion,
        );

        let mut unknown_channel = builtin_manifest().expect("manifest");
        unknown_channel.rule_channel = "nightly".to_string();
        assert_error_kind(
            validate_manifest(&unknown_channel),
            ResolveError::UnsupportedRuleChannel("nightly".to_string()),
        );

        let mut missing_channel = builtin_manifest().expect("manifest");
        missing_channel.rule_channel = String::new();
        assert_error_kind(
            validate_manifest(&missing_channel),
            ResolveError::MissingRuleChannel,
        );
    }

    #[test]
    fn validation_accepts_current_and_older_minimum_app_versions() {
        for minimum_app_version in [env!("CARGO_PKG_VERSION"), "0.3.0", "0.3.1-alpha.1"] {
            let mut manifest = builtin_manifest().expect("manifest");
            manifest.minimum_app_version = minimum_app_version.to_string();

            validate_manifest(&manifest).expect("manifest minimum should be compatible");
        }
    }

    #[test]
    fn validation_rejects_invalid_artifact_definitions() {
        let mut empty_id = builtin_manifest().expect("manifest");
        empty_id.artifacts[0].id = String::new();
        assert_error_kind(
            validate_manifest(&empty_id),
            ResolveError::MissingArtifactId,
        );

        let mut duplicate_id = builtin_manifest().expect("manifest");
        duplicate_id
            .artifacts
            .push(duplicate_id.artifacts[0].clone());
        assert_error_kind(
            validate_manifest(&duplicate_id),
            ResolveError::DuplicateArtifactId("sodium".to_string()),
        );

        let mut missing_project = builtin_manifest().expect("manifest");
        missing_project.artifacts[0].source.project_id = String::new();
        assert_error_kind(
            validate_manifest(&missing_project),
            ResolveError::MissingArtifactProjectId("sodium".to_string()),
        );

        let mut missing_slug = builtin_manifest().expect("manifest");
        missing_slug.artifacts[0].source.slug = String::new();
        assert_error_kind(
            validate_manifest(&missing_slug),
            ResolveError::MissingArtifactSlug("sodium".to_string()),
        );

        let mut user_owned = builtin_manifest().expect("manifest");
        user_owned.artifacts[0].ownership_class = OwnershipClass::UserManaged;
        assert_error_kind(
            validate_manifest(&user_owned),
            ResolveError::InvalidArtifactOwnership("sodium".to_string()),
        );
    }

    #[test]
    fn manifest_rejects_unverifiable_artifact_publisher_signature_fields() {
        let error = serde_json::from_value::<Manifest>(serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-04-02T00:00:00Z",
            "minimum_app_version": "0.3.1",
            "rule_channel": "bundled",
            "artifacts": [{
                "id": "sodium",
                "type": "mod",
                "source": {
                    "provider": "modrinth",
                    "project_id": "sodium",
                    "slug": "sodium"
                },
                "checksum_policy": "provider_sha512",
                "ownership_class": "composition_managed",
                "publisher_signature": {
                    "algorithm": "ed25519",
                    "signature": "00"
                }
            }],
            "compositions": [],
            "emergency_disables": []
        }))
        .expect_err("unmodeled artifact signature fields should be invalid current schema");

        assert!(error.to_string().contains("publisher_signature"));
    }

    #[test]
    fn validation_rejects_invalid_managed_mod_artifact_references() {
        let mut missing_reference = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut missing_reference).artifact_id = String::new();
        assert_error_kind(
            validate_manifest(&missing_reference),
            ResolveError::MissingManagedModArtifactId,
        );

        let mut unknown_reference = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut unknown_reference).artifact_id = "missing".to_string();
        assert_error_kind(
            validate_manifest(&unknown_reference),
            ResolveError::UnknownManagedModArtifact("missing".to_string()),
        );

        let mut project_mismatch = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut project_mismatch).project_id = "other-project".to_string();
        assert_error_kind(
            validate_manifest(&project_mismatch),
            ResolveError::ManagedModProjectMismatch {
                artifact_id: "sodium".to_string(),
                expected: "sodium".to_string(),
                actual: "other-project".to_string(),
            },
        );

        let mut slug_mismatch = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut slug_mismatch).slug = "other-slug".to_string();
        assert_error_kind(
            validate_manifest(&slug_mismatch),
            ResolveError::ManagedModSlugMismatch {
                artifact_id: "sodium".to_string(),
                expected: "sodium".to_string(),
                actual: "other-slug".to_string(),
            },
        );
    }

    #[test]
    fn validation_accepts_builtin_manifest_and_valid_managed_mod_version_ranges() {
        let manifest = builtin_manifest().expect("manifest");
        validate_manifest(&manifest).expect("built-in manifest should validate");

        for version_range in ["", ">=1.16 <1.19.4", ">=1.20.1", "1.20.4", "24w14a"] {
            let mut manifest = builtin_manifest().expect("manifest");
            first_managed_mod_mut(&mut manifest).version_range = version_range.to_string();

            validate_manifest(&manifest).expect("managed mod version_range should validate");
        }
    }

    #[test]
    fn validation_rejects_malformed_managed_mod_version_ranges() {
        for version_range in [">=", ">=not-a-version"] {
            let mut manifest = builtin_manifest().expect("manifest");
            let artifact_id = {
                let managed_mod = first_managed_mod_mut(&mut manifest);
                managed_mod.version_range = version_range.to_string();
                managed_mod.artifact_id.clone()
            };

            assert_error_kind(
                validate_manifest(&manifest),
                ResolveError::InvalidManagedModVersionRange {
                    artifact_id,
                    version_range: version_range.to_string(),
                },
            );
        }
    }

    #[test]
    fn validation_accepts_default_and_valid_managed_mod_hardware_requirements() {
        let mut default_manifest = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut default_manifest).hardware_req =
            Some(HardwareRequirement::default());
        validate_manifest(&default_manifest).expect("default hardware_req should validate");

        let mut nvidium_manifest = builtin_manifest().expect("manifest");
        let nvidium = nvidium_managed_mod_mut(&mut nvidium_manifest);
        nvidium.hardware_req = Some(HardwareRequirement {
            gpu_vendor: "nvidia".to_string(),
            gpu_arch_min: 2,
            ..HardwareRequirement::default()
        });
        validate_manifest(&nvidium_manifest).expect("Nvidium hardware_req should validate");
    }

    #[test]
    fn validation_rejects_negative_managed_mod_hardware_requirements() {
        for (field, value) in [
            ("gpu_arch_min", -1),
            ("min_ram_mb", -2048),
            ("min_cores", -2),
        ] {
            let mut manifest = builtin_manifest().expect("manifest");
            let artifact_id = {
                let managed_mod = first_managed_mod_mut(&mut manifest);
                let mut requirement = HardwareRequirement::default();
                match field {
                    "gpu_arch_min" => requirement.gpu_arch_min = value,
                    "min_ram_mb" => requirement.min_ram_mb = value,
                    "min_cores" => requirement.min_cores = value,
                    _ => unreachable!("test field should be covered"),
                }
                managed_mod.hardware_req = Some(requirement);
                managed_mod.artifact_id.clone()
            };

            assert_error_kind(
                validate_manifest(&manifest),
                ResolveError::InvalidManagedModHardwareRequirement {
                    artifact_id,
                    field,
                    value,
                },
            );
        }
    }

    #[test]
    fn validation_accepts_builtin_manifest_and_undeclared_mutual_exclusions() {
        let mut manifest = builtin_manifest().expect("manifest");
        let nvidium = nvidium_managed_mod_mut(&mut manifest);
        assert_eq!(nvidium.mutual_exclusions, vec!["iris".to_string()]);
        nvidium.mutual_exclusions.push("sodium-extra".to_string());

        validate_manifest(&manifest).expect("mutual exclusions need not be managed artifacts");
    }

    #[test]
    fn validation_rejects_blank_managed_mod_mutual_exclusions() {
        for exclusion in ["", " \t "] {
            let mut manifest = builtin_manifest().expect("manifest");
            let artifact_id = {
                let managed_mod = first_managed_mod_mut(&mut manifest);
                managed_mod.mutual_exclusions = vec![exclusion.to_string()];
                managed_mod.artifact_id.clone()
            };

            assert_error_kind(
                validate_manifest(&manifest),
                ResolveError::InvalidManagedModMutualExclusion {
                    artifact_id,
                    field: "entry",
                    value: exclusion.to_string(),
                },
            );
        }
    }

    #[test]
    fn validation_rejects_whitespace_padded_managed_mod_mutual_exclusions() {
        for exclusion in [" iris", "iris "] {
            let mut manifest = builtin_manifest().expect("manifest");
            let artifact_id = {
                let managed_mod = first_managed_mod_mut(&mut manifest);
                managed_mod.mutual_exclusions = vec![exclusion.to_string()];
                managed_mod.artifact_id.clone()
            };

            assert_error_kind(
                validate_manifest(&manifest),
                ResolveError::InvalidManagedModMutualExclusion {
                    artifact_id,
                    field: "entry",
                    value: exclusion.to_string(),
                },
            );
        }
    }

    #[test]
    fn validation_rejects_duplicate_managed_mod_mutual_exclusions_case_insensitively() {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            managed_mod.mutual_exclusions = vec!["iris".to_string(), "IRIS".to_string()];
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModMutualExclusion {
                artifact_id,
                field: "duplicate",
                value: "IRIS".to_string(),
            },
        );
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
        for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
            assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
        }
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

    #[test]
    fn artifact_disable_targets_declared_artifact_aliases() {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.artifacts[0].id = "sodium-artifact".to_string();
        for composition in &mut manifest.compositions {
            for managed_mod in &mut composition.mods {
                if managed_mod.artifact_id == "sodium" {
                    managed_mod.artifact_id = "sodium-artifact".to_string();
                }
            }
        }
        manifest
            .emergency_disables
            .push(test_artifact_disable("hold-sodium-alias", "sodium"));
        validate_manifest(&manifest).expect("declared source alias should validate");

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

        assert!(
            plan.mods
                .iter()
                .all(|managed_mod| managed_mod.slug != "sodium")
        );
        assert!(plan.warnings.iter().any(|warning| {
            warning.contains("sodium skipped by emergency disable hold-sodium-alias")
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

    fn first_managed_mod_mut(manifest: &mut Manifest) -> &mut ManagedMod {
        manifest
            .compositions
            .iter_mut()
            .find_map(|composition| composition.mods.first_mut())
            .expect("test manifest should include a managed mod")
    }

    fn nvidium_managed_mod_mut(manifest: &mut Manifest) -> &mut ManagedMod {
        manifest
            .compositions
            .iter_mut()
            .flat_map(|composition| &mut composition.mods)
            .find(|managed_mod| managed_mod.slug == "nvidium")
            .expect("test manifest should include nvidium")
    }

    fn count_mods_with_slug(mods: &[ManagedMod], slug: &str) -> usize {
        mods.iter()
            .filter(|managed_mod| managed_mod.slug == slug)
            .count()
    }

    fn fabric_plan(game_version: &str, hardware: HardwareProfile) -> CompositionPlan {
        let manifest = builtin_manifest().expect("manifest");
        resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware,
                installed_mods: Vec::new(),
            },
        )
    }

    fn nvidia_turing_hardware() -> HardwareProfile {
        HardwareProfile {
            gpu_vendor: "nvidia".to_string(),
            gpu_arch: 2,
            ..HardwareProfile::default()
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
