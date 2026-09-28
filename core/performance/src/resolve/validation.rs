use super::app_version::validate_app_version_compatibility;
use super::model::ResolveError;
use super::version::{parse_version, split_range_condition};
use crate::types::{
    EmergencyDisableTarget, ManagedArtifactDefinition, ManagedMod, Manifest, OwnershipClass,
};

const BUILTIN_CATALOG: &str = include_str!("../catalog.json");
const KNOWN_RULE_CHANNELS: &[&str] = &["bundled", "local", "remote"];
const MAX_MANIFEST_ITEMS: usize = 256;
const MAX_FILTER_ITEMS: usize = 64;
const MAX_ID_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_VERSION_RANGE_CHARS: usize = 256;
const MAX_EXACT_GAME_VERSION_CHARS: usize = 64;
const MAX_HARDWARE_VALUE: i32 = 1_048_576;
const MODRINTH_PROJECT_ID_CHARS: usize = 8;

pub const PERFORMANCE_MANIFEST_SCHEMA_VERSION: i32 = 3;

pub fn builtin_manifest() -> Result<Manifest, ResolveError> {
    let manifest = serde_json::from_str::<Manifest>(BUILTIN_CATALOG)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), ResolveError> {
    if manifest.schema_version != PERFORMANCE_MANIFEST_SCHEMA_VERSION {
        return Err(ResolveError::UnsupportedSchema);
    }
    validate_text(&manifest.generated_at, 64, "generated_at")?;
    chrono::DateTime::parse_from_rfc3339(&manifest.generated_at)
        .map_err(|_| ResolveError::ManifestBound("generated_at"))?;
    validate_text(&manifest.minimum_app_version, 64, "minimum_app_version")?;
    validate_text(&manifest.rule_channel, 32, "rule_channel")?;
    validate_count(
        manifest.artifacts.len(),
        MAX_MANIFEST_ITEMS,
        "artifact count",
    )?;
    validate_count(
        manifest.compositions.len(),
        MAX_MANIFEST_ITEMS,
        "composition count",
    )?;
    validate_count(
        manifest.emergency_disables.len(),
        MAX_MANIFEST_ITEMS,
        "emergency disable count",
    )?;
    validate_app_version_compatibility(&manifest.minimum_app_version)?;
    validate_rule_channel(&manifest.rule_channel)?;
    let artifacts = validate_artifacts(&manifest.artifacts)?;

    let mut ids = std::collections::HashSet::new();
    for composition in &manifest.compositions {
        validate_text(&composition.id, MAX_ID_CHARS, "composition id")?;
        validate_text(
            &composition.display_name,
            MAX_NAME_CHARS,
            "composition display name",
        )?;
        validate_text(
            &composition.description,
            MAX_DESCRIPTION_CHARS,
            "composition description",
        )?;
        validate_optional_text(
            &composition.jvm_preset,
            MAX_NAME_CHARS,
            "composition jvm preset",
        )?;
        validate_count(
            composition.families.len(),
            MAX_FILTER_ITEMS,
            "composition family count",
        )?;
        validate_count(
            composition.loaders.len(),
            MAX_FILTER_ITEMS,
            "composition loader count",
        )?;
        validate_count(
            composition.mods.len(),
            MAX_MANIFEST_ITEMS,
            "composition mod count",
        )?;
        validate_unique_text(&composition.loaders, "composition loaders")?;
        for loader in &composition.loaders {
            validate_text(loader, MAX_ID_CHARS, "composition loader")?;
        }
        if composition.id.is_empty() {
            return Err(ResolveError::MissingCompositionId);
        }
        if !ids.insert(composition.id.clone()) {
            return Err(ResolveError::DuplicateCompositionId(composition.id.clone()));
        }
    }

    for composition in &manifest.compositions {
        for managed_mod in &composition.mods {
            validate_managed_mod_artifact(managed_mod, &artifacts)?;
        }
    }

    let artifact_targets = declared_artifact_targets(&manifest.artifacts);

    let mut disable_ids = std::collections::HashSet::new();
    for disable in &manifest.emergency_disables {
        validate_text(&disable.id, MAX_ID_CHARS, "emergency disable id")?;
        validate_text(&disable.target_id, MAX_ID_CHARS, "emergency disable target")?;
        validate_text(
            &disable.reason,
            MAX_DESCRIPTION_CHARS,
            "emergency disable reason",
        )?;
        validate_count(
            disable.families.len(),
            MAX_FILTER_ITEMS,
            "emergency family count",
        )?;
        validate_count(
            disable.loaders.len(),
            MAX_FILTER_ITEMS,
            "emergency loader count",
        )?;
        validate_count(
            disable.tiers.len(),
            MAX_FILTER_ITEMS,
            "emergency tier count",
        )?;
        validate_unique_text(&disable.loaders, "emergency loaders")?;
        for loader in &disable.loaders {
            validate_text(loader, MAX_ID_CHARS, "emergency loader")?;
        }
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
        validate_text(&artifact.id, MAX_ID_CHARS, "artifact id")?;
        validate_text(
            &artifact.source.project_id,
            MAX_ID_CHARS,
            "artifact project id",
        )?;
        validate_text(&artifact.source.slug, MAX_ID_CHARS, "artifact slug")?;
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
        validate_modrinth_project_id(&artifact.source.project_id, "artifact Modrinth project id")?;
        if artifact.source.slug.trim().is_empty() {
            return Err(ResolveError::MissingArtifactSlug(artifact.id.clone()));
        }
        if artifact.source.slug.trim() != artifact.source.slug {
            return Err(ResolveError::ManifestBound("artifact slug padding"));
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
    validate_text(
        &managed_mod.artifact_id,
        MAX_ID_CHARS,
        "managed mod artifact id",
    )?;
    validate_text(
        &managed_mod.project_id,
        MAX_ID_CHARS,
        "managed mod project id",
    )?;
    validate_text(&managed_mod.slug, MAX_ID_CHARS, "managed mod slug")?;
    validate_text(&managed_mod.name, MAX_NAME_CHARS, "managed mod name")?;
    validate_optional_text(
        &managed_mod.version_range,
        MAX_VERSION_RANGE_CHARS,
        "managed mod version range",
    )?;
    validate_count(
        managed_mod.exact_game_versions.len(),
        MAX_FILTER_ITEMS,
        "managed mod exact game version count",
    )?;
    if managed_mod.artifact_id.trim().is_empty() {
        return Err(ResolveError::MissingManagedModArtifactId);
    }
    let Some(artifact) = artifacts.get(&managed_mod.artifact_id.to_lowercase()) else {
        return Err(ResolveError::UnknownManagedModArtifact(
            managed_mod.artifact_id.clone(),
        ));
    };
    validate_modrinth_project_id(&managed_mod.project_id, "managed mod Modrinth project id")?;
    if managed_mod.slug.trim() != managed_mod.slug {
        return Err(ResolveError::ManifestBound("managed mod slug padding"));
    }
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
    validate_managed_mod_exact_game_versions(managed_mod)?;
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

fn validate_managed_mod_exact_game_versions(managed_mod: &ManagedMod) -> Result<(), ResolveError> {
    if !managed_mod.version_range.trim().is_empty() && !managed_mod.exact_game_versions.is_empty() {
        return Err(ResolveError::ConflictingManagedModVersionSelectors {
            artifact_id: managed_mod.artifact_id.clone(),
        });
    }

    let mut seen = std::collections::HashSet::new();
    for game_version in &managed_mod.exact_game_versions {
        validate_text(
            game_version,
            MAX_EXACT_GAME_VERSION_CHARS,
            "managed mod exact game version",
        )?;
        if game_version.is_empty()
            || game_version.trim() != game_version
            || parse_version(game_version).is_err()
        {
            return Err(ResolveError::InvalidManagedModExactGameVersion {
                artifact_id: managed_mod.artifact_id.clone(),
                game_version: game_version.clone(),
            });
        }
        if !seen.insert(game_version) {
            return Err(ResolveError::DuplicateManagedModExactGameVersion {
                artifact_id: managed_mod.artifact_id.clone(),
                game_version: game_version.clone(),
            });
        }
    }
    Ok(())
}

fn validate_managed_mod_hardware_req(managed_mod: &ManagedMod) -> Result<(), ResolveError> {
    let Some(requirement) = &managed_mod.hardware_req else {
        return Ok(());
    };
    validate_optional_text(
        &requirement.gpu_vendor,
        64,
        "managed mod hardware gpu vendor",
    )?;
    for (field, value) in [
        ("gpu_arch_min", requirement.gpu_arch_min),
        ("min_ram_mb", requirement.min_ram_mb),
        ("min_cores", requirement.min_cores),
    ] {
        if !(0..=MAX_HARDWARE_VALUE).contains(&value) {
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
    validate_count(
        managed_mod.mutual_exclusions.len(),
        MAX_FILTER_ITEMS,
        "managed mod mutual exclusion count",
    )?;
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
        validate_text(exclusion, MAX_ID_CHARS, "managed mod mutual exclusion")?;
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

fn validate_count(actual: usize, max: usize, field: &'static str) -> Result<(), ResolveError> {
    if actual > max {
        return Err(ResolveError::ManifestBound(field));
    }
    Ok(())
}

fn validate_text(value: &str, max: usize, field: &'static str) -> Result<(), ResolveError> {
    if value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(ResolveError::ManifestBound(field));
    }
    Ok(())
}

fn validate_modrinth_project_id(project_id: &str, field: &'static str) -> Result<(), ResolveError> {
    if project_id.len() != MODRINTH_PROJECT_ID_CHARS
        || !project_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(ResolveError::ManifestBound(field));
    }
    Ok(())
}

fn validate_optional_text(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), ResolveError> {
    if value.is_empty() {
        return Ok(());
    }
    validate_text(value, max, field)
}

fn validate_unique_text(values: &[String], field: &'static str) -> Result<(), ResolveError> {
    let mut seen = std::collections::HashSet::new();
    if values
        .iter()
        .any(|value| !seen.insert(value.to_ascii_lowercase()))
    {
        return Err(ResolveError::ManifestBound(field));
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
    }
    targets
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
