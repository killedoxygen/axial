//! Composition health derived from exact admitted leaf inspection.

use super::storage::ManagedStorageDirectory;
use crate::performance::model::*;
use std::collections::BTreeSet;

pub(crate) fn derive_health(
    state: Option<&CompositionState>,
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
    instance_mods: Option<&ManagedStorageDirectory>,
) -> (BundleHealth, Vec<String>) {
    let Some(state) = state else {
        return (BundleHealth::Disabled, Vec::new());
    };

    if let Err(error) = super::plan::validate_state_graph(state) {
        return (
            BundleHealth::Invalid,
            vec![format!("managed composition graph is invalid: {error}")],
        );
    }

    if let Some(plan) = plan
        && (!declarative_plan_matches_state(plan, state)
            || expected_game_version.is_some_and(|expected| expected != state.game_version))
    {
        return (
            BundleHealth::Invalid,
            vec!["managed composition does not match the current declarative plan".to_string()],
        );
    }

    let Some(instance_mods) = instance_mods else {
        return (
            BundleHealth::Invalid,
            vec!["managed composition storage authority is unavailable".to_string()],
        );
    };
    let mut warnings = Vec::new();
    for installed in &state.installed_mods {
        match super::state::managed_artifact_matches(instance_mods, installed) {
            Ok(true) => {}
            Ok(false) => warnings.push(format!(
                "{} is missing or failed exact integrity validation",
                installed.filename
            )),
            Err(_) => warnings.push(format!("{} could not be validated", installed.filename)),
        }
    }
    if warnings.is_empty() {
        (BundleHealth::Healthy, warnings)
    } else {
        (BundleHealth::Invalid, warnings)
    }
}

fn declarative_plan_matches_state(plan: &CompositionPlan, state: &CompositionState) -> bool {
    let expected_roots = plan
        .mods
        .iter()
        .map(|managed_mod| managed_mod.project_id.as_str())
        .collect::<BTreeSet<_>>();
    let installed_roots = state
        .installed_mods
        .iter()
        .filter(|installed| installed.role == ManagedArtifactRole::Root)
        .map(|installed| installed.project_id.as_str())
        .collect::<BTreeSet<_>>();
    plan.composition_id == state.composition_id
        && plan.family == state.family
        && plan.tier == state.tier
        && plan.loader == state.loader
        && expected_roots == installed_roots
}
