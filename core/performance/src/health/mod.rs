use crate::types::{CompositionPlan, CompositionState, ManagedArtifactRole};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleHealth {
    Healthy,
    Disabled,
    Invalid,
}

pub(crate) fn derive_health(
    state: Option<&CompositionState>,
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
    instance_mods_dir: &Path,
) -> (BundleHealth, Vec<String>) {
    let Some(state) = state else {
        return (BundleHealth::Disabled, Vec::new());
    };

    if let Err(error) = crate::install::plan::validate_state_graph(state) {
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

    let mut warnings = Vec::new();
    for installed in &state.installed_mods {
        match crate::state::managed_artifact_matches(instance_mods_dir, installed) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_disabled() {
        let (health, warnings) = derive_health(None, None, None, Path::new("unused"));
        assert_eq!(health, BundleHealth::Disabled);
        assert!(warnings.is_empty());
    }
}
