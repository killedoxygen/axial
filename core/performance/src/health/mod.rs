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

    for installed in &state.installed_mods {
        if !installed.integrity.sha512_verified {
            warnings.push(format!(
                "{} lacks verified SHA-512 integrity evidence",
                installed.filename
            ));
        }
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
    }

    if !warnings.is_empty() {
        return (BundleHealth::Degraded, warnings);
    }

    if let Some(plan) = plan
        && tier_rank(state.tier) < tier_rank(plan.tier)
    {
        warnings.push("managed composition resolved to a lower tier than expected".to_string());
        return (BundleHealth::Fallback, warnings);
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
        CompositionPlan, CompositionState, CompositionTier, InstalledMod, ManagedArtifactIntegrity,
        ManagedArtifactProvider, ManagedArtifactSource, OwnershipClass, PerformanceMode,
        VersionFamily,
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
            source: modrinth_source(),
            integrity: unverified_integrity(),
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
            source: modrinth_source(),
            integrity: verified_integrity(),
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
    fn safe_install_failure_evidence_keeps_health_warning_product_safe() {
        let root = test_root("safe-install-failure-warning");
        let mut state = test_state(Vec::new());
        state.failure_count = 1;
        state.last_failure = "managed artifact install failed".to_string();

        let (health, warnings) = derive_health(Some(&state), None, &root);
        let warning_text = warnings.join("\n");

        assert_eq!(health, BundleHealth::Degraded);
        assert_eq!(
            warnings,
            vec!["1 managed mod install failure(s): managed artifact install failed"]
        );
        for detail in [
            "https://cdn.modrinth.com/data/private/sodium-secret.jar?token=secret",
            "sodium-secret.jar",
            "/home/zero/.minecraft/mods/private/sodium-secret.jar",
            "C:\\Users\\Zero\\AppData\\Roaming\\.minecraft\\mods\\sodium-secret.jar",
            "error decoding response body at line 1 column 2",
            "No such file or directory (os error 2)",
        ] {
            assert!(!state.last_failure.contains(detail), "{detail}");
            assert!(!warning_text.contains(detail), "{detail}");
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lower_installed_tier_than_current_plan_is_fallback() {
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

        assert_eq!(health, BundleHealth::Fallback);
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

    #[test]
    fn unverified_managed_artifacts_are_degraded() {
        let root = test_root("unverified-integrity");
        fs::write(root.join("sodium.jar"), b"jar").expect("write managed mod");
        let state = test_state(vec![InstalledMod {
            project_id: "sodium".to_string(),
            version_id: "version".to_string(),
            filename: "sodium.jar".to_string(),
            ownership_class: OwnershipClass::CompositionManaged,
            source: modrinth_source(),
            integrity: unverified_integrity(),
        }]);

        let (health, warnings) = derive_health(Some(&state), None, &root);

        assert_eq!(health, BundleHealth::Degraded);
        assert_eq!(
            warnings,
            vec!["sodium.jar lacks verified SHA-512 integrity evidence"]
        );

        let _ = fs::remove_dir_all(root);
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

    fn modrinth_source() -> ManagedArtifactSource {
        ManagedArtifactSource {
            provider: ManagedArtifactProvider::Modrinth,
        }
    }

    fn verified_integrity() -> ManagedArtifactIntegrity {
        ManagedArtifactIntegrity {
            sha512: valid_sha512(),
            sha512_verified: true,
        }
    }

    fn unverified_integrity() -> ManagedArtifactIntegrity {
        ManagedArtifactIntegrity {
            sha512: String::new(),
            sha512_verified: false,
        }
    }

    fn valid_sha512() -> String {
        "a".repeat(128)
    }
}
