use super::manager::{ACTIVE_RULES_LOCK_INVARIANT, PerformanceManager};
use crate::types::{
    CompositionPlan, CompositionState, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
    ManagedMod, Manifest, VersionFamily,
};
use chrono::Utc;
use tracing::warn;

const MIN_USEFUL_MANAGED_INSTALLS: usize = 2;
const MAX_INSTALL_FALLBACK_ATTEMPTS: usize = 4;

impl PerformanceManager {
    pub(super) fn install_attempt_plans(&self, plan: &CompositionPlan) -> Vec<CompositionPlan> {
        let active = self.active.read().expect(ACTIVE_RULES_LOCK_INVARIANT);
        let manifest = &active.manifest;
        let mut plans = vec![plan.clone()];
        let mut seen = std::collections::HashSet::new();
        if !plan.composition_id.is_empty() {
            seen.insert(plan.composition_id.clone());
        }

        for (chain_index, fallback_id) in plan
            .fallback_chain
            .iter()
            .enumerate()
            .filter(|(_, fallback_id)| !fallback_id.trim().is_empty())
            .take(MAX_INSTALL_FALLBACK_ATTEMPTS.saturating_sub(1))
        {
            if !seen.insert(fallback_id.clone()) {
                warn!("performance fallback cycle ignored at {}", fallback_id);
                continue;
            }

            let Some(definition) = manifest
                .compositions
                .iter()
                .find(|definition| definition.id == *fallback_id)
            else {
                warn!(
                    "performance fallback composition {} is missing",
                    fallback_id
                );
                continue;
            };

            if !definition.families.contains(&plan.family)
                || !definition
                    .loaders
                    .iter()
                    .any(|loader| loader.eq_ignore_ascii_case(&plan.loader))
            {
                warn!(
                    "performance fallback composition {} does not apply to {:?}/{}",
                    fallback_id, plan.family, plan.loader
                );
                continue;
            }

            if active_composition_disable(manifest, definition, plan.family, &plan.loader).is_some()
            {
                warn!(
                    "performance fallback composition {} is emergency disabled",
                    fallback_id
                );
                continue;
            }

            let mods = definition
                .mods
                .iter()
                .filter_map(|managed_mod| {
                    let admitted_mod = plan.mods.iter().find(|admitted_mod| {
                        admitted_mod
                            .artifact_id
                            .eq_ignore_ascii_case(&managed_mod.artifact_id)
                    })?;
                    if admitted_mod != managed_mod {
                        warn!(
                            "performance fallback artifact {} has a divergent declaration",
                            managed_mod.artifact_id
                        );
                        return None;
                    }
                    active_artifact_disable(
                        manifest,
                        managed_mod,
                        plan.family,
                        &plan.loader,
                        definition.tier,
                    )
                    .is_none()
                    .then(|| managed_mod.clone())
                })
                .collect();

            plans.push(CompositionPlan {
                composition_id: definition.id.clone(),
                family: plan.family,
                loader: plan.loader.clone(),
                mode: plan.mode,
                tier: definition.tier,
                mods,
                jvm_preset: definition.jvm_preset.clone(),
                fallback_chain: plan
                    .fallback_chain
                    .iter()
                    .skip(chain_index + 1)
                    .cloned()
                    .collect(),
                warnings: plan.warnings.clone(),
                fallback_reason: format!(
                    "install fallback from {} after severe managed install failure",
                    plan.composition_id
                ),
            });
        }

        plans
    }
}

pub(super) fn empty_state(plan: &CompositionPlan) -> CompositionState {
    CompositionState {
        composition_id: plan.composition_id.clone(),
        tier: plan.tier,
        installed_mods: Vec::new(),
        installed_at: Utc::now().to_rfc3339(),
        failure_count: 0,
        last_failure: String::new(),
    }
}

pub(super) fn severe_install_failure(plan: &CompositionPlan, state: &CompositionState) -> bool {
    !plan.mods.is_empty()
        && state.failure_count > 0
        && state.installed_mods.len() < MIN_USEFUL_MANAGED_INSTALLS
}

fn active_composition_disable<'a>(
    manifest: &'a Manifest,
    definition: &crate::types::CompositionDef,
    family: VersionFamily,
    loader: &str,
) -> Option<&'a EmergencyDisable> {
    manifest.emergency_disables.iter().find(|disable| {
        disable.target == EmergencyDisableTarget::Composition
            && disable.target_id == definition.id
            && disable_applies(disable, family, loader, definition.tier)
    })
}

pub(super) fn active_artifact_disable<'a>(
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
