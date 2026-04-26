use crate::types::{CompositionPlan, CompositionState, CompositionTier};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleHealth {
    Healthy,
    Degraded,
    Fallback,
    Disabled,
    Invalid,
}

pub fn derive_health(
    state: Option<&CompositionState>,
    plan: Option<&CompositionPlan>,
    instance_mods_dir: &Path,
) -> (BundleHealth, Vec<String>) {
    let Some(state) = state else {
        return (BundleHealth::Disabled, Vec::new());
    };

    let mut warnings = Vec::new();
    for installed in &state.installed_mods {
        if !instance_mods_dir.join(&installed.filename).is_file() {
            warnings.push(format!("{} missing from mods folder", installed.filename));
        }
    }
    if !warnings.is_empty() {
        return (BundleHealth::Invalid, warnings);
    }

    if let Some(plan) = plan
        && tier_rank(state.tier) < tier_rank(plan.tier)
    {
        warnings.push("managed composition resolved to a lower tier than expected".to_string());
        return (BundleHealth::Degraded, warnings);
    }

    (BundleHealth::Healthy, warnings)
}

fn tier_rank(tier: CompositionTier) -> i32 {
    match tier {
        CompositionTier::Extended => 3,
        CompositionTier::Core => 2,
        CompositionTier::VanillaEnhanced => 1,
    }
}
