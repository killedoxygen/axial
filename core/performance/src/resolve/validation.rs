use super::app_version::validate_app_version_compatibility;
use super::model::ResolveError;
use super::version::{parse_version, split_range_condition};
use crate::types::{
    EmergencyDisableTarget, ManagedArtifactDefinition, ManagedMod, Manifest, OwnershipClass,
};

const BUILTIN_CATALOG: &str = include_str!("../catalog.json");
const KNOWN_RULE_CHANNELS: &[&str] = &["bundled", "local", "remote"];

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
