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

    if state.failure_count > 0 {
        warnings.push(if state.last_failure.trim().is_empty() {
            format!("{} managed mod install failure(s)", state.failure_count)
        } else {
            format!(
                "{} managed mod install failure(s): {}",
                state.failure_count, state.last_failure
            )
        });
        return (BundleHealth::Degraded, warnings);
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

#[cfg(test)]
mod tests {
    use super::{BundleHealth, derive_health};
    use crate::types::{
        CompositionPlan, CompositionState, CompositionTier, InstalledMod, OwnershipClass,
        PerformanceMode, VersionFamily,
    };
    use std::{fs, path::PathBuf};

    #[test]
    fn missing_state_is_disabled_without_warnings() {
        let root = test_root("missing-state");
        let (health, warnings) = derive_health(None, None, &root);

        assert_eq!(health, BundleHealth::Disabled);
        assert!(warnings.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_managed_file_is_invalid() {
        let root = test_root("missing-file");
        let state = test_state(vec![InstalledMod {
            project_id: "sodium".to_string(),
            version_id: "version".to_string(),
            filename: "sodium.jar".to_string(),
            ownership_class: OwnershipClass::CompositionManaged,
            sha512: String::new(),
        }]);

        let (health, warnings) = derive_health(Some(&state), None, &root);

        assert_eq!(health, BundleHealth::Invalid);
        assert_eq!(warnings, vec!["sodium.jar missing from mods folder"]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn saved_install_failures_are_degraded() {
        let root = test_root("install-failure");
        fs::write(root.join("sodium.jar"), b"jar").expect("write managed mod");
        let mut state = test_state(vec![InstalledMod {
            project_id: "sodium".to_string(),
            version_id: "version".to_string(),
            filename: "sodium.jar".to_string(),
            ownership_class: OwnershipClass::CompositionManaged,
            sha512: String::new(),
        }]);
        state.failure_count = 1;
        state.last_failure = "no compatible versions found for lithium".to_string();

        let (health, warnings) = derive_health(Some(&state), None, &root);

        assert_eq!(health, BundleHealth::Degraded);
        assert_eq!(
            warnings,
            vec!["1 managed mod install failure(s): no compatible versions found for lithium"]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lower_installed_tier_than_current_plan_is_degraded() {
        let root = test_root("lower-tier");
        let state = test_state(Vec::new());
        let plan = CompositionPlan {
            composition_id: "extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: Vec::new(),
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let (health, warnings) = derive_health(Some(&state), Some(&plan), &root);

        assert_eq!(health, BundleHealth::Degraded);
        assert_eq!(
            warnings,
            vec!["managed composition resolved to a lower tier than expected"]
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_state(installed_mods: Vec<InstalledMod>) -> CompositionState {
        CompositionState {
            composition_id: "core".to_string(),
            tier: CompositionTier::Core,
            installed_mods,
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-performance-health-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
