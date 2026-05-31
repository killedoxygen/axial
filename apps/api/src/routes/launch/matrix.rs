use serde::Serialize;

const MATRIX_SCHEMA: &str = "croopor.launch.benchmark.matrix";
const MATRIX_SCHEMA_VERSION: u32 = 1;
const MAX_MATRIX_JSON_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkMatrix {
    pub schema: &'static str,
    pub schema_version: u32,
    pub modes: Vec<BenchmarkModeDescriptor>,
    pub run_types: Vec<BenchmarkRunTypeDescriptor>,
    pub profiles: Vec<BenchmarkProfileDescriptor>,
    pub representative_targets: Vec<BenchmarkTargetDescriptor>,
    pub limits: BenchmarkMatrixLimits,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkModeDescriptor {
    pub id: &'static str,
    pub description: &'static str,
    pub intended_use: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkRunTypeDescriptor {
    pub id: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkProfileDescriptor {
    pub id: &'static str,
    pub scenario: &'static str,
    pub description: &'static str,
    pub intended_use: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkTargetDescriptor {
    pub id: &'static str,
    pub family: &'static str,
    pub version: &'static str,
    pub loader: &'static str,
    pub profile: &'static str,
    pub run_type: &'static str,
    pub description: &'static str,
    pub intended_use: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BenchmarkMatrixLimits {
    pub max_payload_bytes: usize,
    pub custom_post_values_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BenchmarkSuiteRunSpec {
    pub profile: &'static str,
    pub run_type: &'static str,
    pub target_id: Option<&'static str>,
}

pub(super) fn benchmark_matrix() -> BenchmarkMatrix {
    BenchmarkMatrix {
        schema: MATRIX_SCHEMA,
        schema_version: MATRIX_SCHEMA_VERSION,
        modes: vec![
            BenchmarkModeDescriptor {
                id: "development",
                description: "Fast local loop with a small scenario subset.",
                intended_use: "Reject obvious regressions while iterating.",
            },
            BenchmarkModeDescriptor {
                id: "qualification",
                description: "Fuller targeted matrix for a family or launch feature.",
                intended_use: "Qualify managed bundles or launch strategy changes before promotion.",
            },
            BenchmarkModeDescriptor {
                id: "release_validation",
                description: "Stable subset for supported default paths.",
                intended_use: "Check for major regressions before a release.",
            },
        ],
        run_types: vec![
            BenchmarkRunTypeDescriptor {
                id: "coldish",
                description: "First launch after normal launcher setup, without relying on repeat-run cache wins.",
            },
            BenchmarkRunTypeDescriptor {
                id: "repeat",
                description: "Subsequent launch of the same target to isolate cache and prewarm benefits.",
            },
        ],
        profiles: vec![
            BenchmarkProfileDescriptor {
                id: "vanilla_baseline",
                scenario: "vanilla baseline",
                description: "Representative vanilla launch with only minimal safe launcher handling.",
                intended_use: "Baseline comparison for managed and current-product behavior.",
            },
            BenchmarkProfileDescriptor {
                id: "managed_default",
                scenario: "managed default",
                description: "Default managed optimization path for the same family or version.",
                intended_use: "Measure the shipped managed path against vanilla baseline.",
            },
            BenchmarkProfileDescriptor {
                id: "degraded_managed_path",
                scenario: "degraded managed path",
                description: "Managed path with optional pieces unavailable or bypassed.",
                intended_use: "Validate fallback behavior and the performance tradeoff.",
            },
            BenchmarkProfileDescriptor {
                id: "legacy_family",
                scenario: "legacy family",
                description: "Representative older Minecraft family workload.",
                intended_use: "Ensure legacy versions are measured within their own family.",
            },
            BenchmarkProfileDescriptor {
                id: "heavy_modded_launch",
                scenario: "heavy modded launch",
                description: "Difficult modded startup workload stressing preparation and early boot.",
                intended_use: "Check launch smoothness under a high-pressure local workload.",
            },
            BenchmarkProfileDescriptor {
                id: "repeat_launch",
                scenario: "repeat launch",
                description: "Same instance launched repeatedly after an initial run.",
                intended_use: "Measure repeat-run cache, prewarm, and managed reuse benefits.",
            },
        ],
        representative_targets: vec![
            BenchmarkTargetDescriptor {
                id: "family_e_fabric_1_16_5_managed",
                family: "E",
                version: "1.16.5",
                loader: "Fabric",
                profile: "managed_default",
                run_type: "coldish",
                description: "Modern-era Fabric managed path anchored to a stable 1.16.5 target.",
                intended_use: "Compare Family E managed startup against vanilla baseline behavior.",
            },
            BenchmarkTargetDescriptor {
                id: "family_e_fabric_1_20_1_managed",
                family: "E",
                version: "1.20.1",
                loader: "Fabric",
                profile: "managed_default",
                run_type: "coldish",
                description: "Modern-era Fabric managed path anchored to a stable 1.20.1 target.",
                intended_use: "Track Family E coverage across a newer stable managed target.",
            },
            BenchmarkTargetDescriptor {
                id: "family_f_modern_fabric_managed",
                family: "F",
                version: "supported modern",
                loader: "Fabric",
                profile: "managed_default",
                run_type: "coldish",
                description: "Current supported modern Fabric managed path without a volatile exact game version.",
                intended_use: "Keep current modern managed coverage visible without promising latest-version semantics.",
            },
            BenchmarkTargetDescriptor {
                id: "family_c_forge_1_12_2_vanilla_baseline",
                family: "C",
                version: "1.12.2",
                loader: "Forge",
                profile: "vanilla_baseline",
                run_type: "coldish",
                description: "Family C Forge 1.12.2 baseline without managed composition mods.",
                intended_use: "Compare the Family C Forge managed path against the same version and loader baseline.",
            },
            BenchmarkTargetDescriptor {
                id: "family_c_forge_1_12_2_family_c_forge_core",
                family: "C",
                version: "1.12.2",
                loader: "Forge",
                profile: "managed_default",
                run_type: "coldish",
                description: "Family C Forge 1.12.2 managed target for the family-c-forge-core composition.",
                intended_use: "Measure the first managed Family C Forge core path against its 1.12.2 baseline.",
            },
            BenchmarkTargetDescriptor {
                id: "legacy_1_8_9_forge_pvp",
                family: "legacy",
                version: "1.8.9",
                loader: "Forge",
                profile: "legacy_family",
                run_type: "coldish",
                description: "Legacy Forge player-versus-player style target with older startup expectations.",
                intended_use: "Keep latency-sensitive legacy coverage represented in the matrix.",
            },
            BenchmarkTargetDescriptor {
                id: "degraded_managed_path",
                family: "E-F",
                version: "supported managed",
                loader: "Fabric",
                profile: "degraded_managed_path",
                run_type: "coldish",
                description: "Managed path with optional acceleration or add-on pieces unavailable.",
                intended_use: "Validate fallback behavior remains measurable and intentionally slower if needed.",
            },
            BenchmarkTargetDescriptor {
                id: "heavy_modded_launch",
                family: "modern",
                version: "supported modern",
                loader: "Fabric",
                profile: "heavy_modded_launch",
                run_type: "coldish",
                description: "Large local modded workload stressing preparation and early boot.",
                intended_use: "Exercise high-pressure modded launch behavior within bounded local evidence.",
            },
            BenchmarkTargetDescriptor {
                id: "repeat_managed_launch",
                family: "E-F",
                version: "supported managed",
                loader: "Fabric",
                profile: "repeat_launch",
                run_type: "repeat",
                description: "Same managed target launched again after an initial successful run.",
                intended_use: "Measure repeat-run cache, prewarm, and managed reuse effects.",
            },
        ],
        limits: BenchmarkMatrixLimits {
            max_payload_bytes: MAX_MATRIX_JSON_BYTES,
            custom_post_values_allowed: true,
        },
    }
}

pub(super) fn benchmark_suite_plan(mode: &str) -> Option<Vec<BenchmarkSuiteRunSpec>> {
    match mode {
        "development" => Some(vec![
            BenchmarkSuiteRunSpec {
                profile: "vanilla_baseline",
                run_type: "coldish",
                target_id: None,
            },
            BenchmarkSuiteRunSpec {
                profile: "managed_default",
                run_type: "repeat",
                target_id: None,
            },
        ]),
        "qualification" => {
            let matrix = benchmark_matrix();
            let mut plan = Vec::with_capacity(matrix.profiles.len() * matrix.run_types.len());
            for profile in &matrix.profiles {
                for run_type in &matrix.run_types {
                    plan.push(BenchmarkSuiteRunSpec {
                        profile: profile.id,
                        run_type: run_type.id,
                        target_id: None,
                    });
                }
            }
            Some(plan)
        }
        "release_validation" => Some(vec![
            BenchmarkSuiteRunSpec {
                profile: "vanilla_baseline",
                run_type: "coldish",
                target_id: Some("family_c_forge_1_12_2_vanilla_baseline"),
            },
            BenchmarkSuiteRunSpec {
                profile: "managed_default",
                run_type: "coldish",
                target_id: Some("family_c_forge_1_12_2_family_c_forge_core"),
            },
            BenchmarkSuiteRunSpec {
                profile: "legacy_family",
                run_type: "coldish",
                target_id: Some("legacy_1_8_9_forge_pvp"),
            },
            BenchmarkSuiteRunSpec {
                profile: "degraded_managed_path",
                run_type: "coldish",
                target_id: Some("degraded_managed_path"),
            },
            BenchmarkSuiteRunSpec {
                profile: "repeat_launch",
                run_type: "repeat",
                target_id: Some("repeat_managed_launch"),
            },
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn benchmark_matrix_contains_stable_mode_and_profile_ids() {
        let matrix = benchmark_matrix();
        let mode_ids = matrix.modes.iter().map(|mode| mode.id).collect::<Vec<_>>();
        let run_type_ids = matrix
            .run_types
            .iter()
            .map(|run_type| run_type.id)
            .collect::<Vec<_>>();
        let profile_ids = matrix
            .profiles
            .iter()
            .map(|profile| profile.id)
            .collect::<Vec<_>>();
        let target_ids = matrix
            .representative_targets
            .iter()
            .map(|target| target.id)
            .collect::<Vec<_>>();

        assert_eq!(
            mode_ids,
            vec!["development", "qualification", "release_validation"]
        );
        assert_eq!(run_type_ids, vec!["coldish", "repeat"]);
        assert_eq!(
            profile_ids,
            vec![
                "vanilla_baseline",
                "managed_default",
                "degraded_managed_path",
                "legacy_family",
                "heavy_modded_launch",
                "repeat_launch",
            ]
        );
        assert_eq!(
            target_ids,
            vec![
                "family_e_fabric_1_16_5_managed",
                "family_e_fabric_1_20_1_managed",
                "family_f_modern_fabric_managed",
                "family_c_forge_1_12_2_vanilla_baseline",
                "family_c_forge_1_12_2_family_c_forge_core",
                "legacy_1_8_9_forge_pvp",
                "degraded_managed_path",
                "heavy_modded_launch",
                "repeat_managed_launch",
            ]
        );
    }

    #[test]
    fn benchmark_matrix_payload_is_bounded_and_descriptor_only() {
        let data = serde_json::to_string(&benchmark_matrix()).expect("serialize matrix");
        let lower_data = data.to_ascii_lowercase();

        assert!(data.len() <= MAX_MATRIX_JSON_BYTES);
        assert!(!data.contains('/'));
        assert!(!data.contains('\\'));
        assert!(!lower_data.contains("java_path"));
        assert!(!lower_data.contains("java"));
        assert!(!lower_data.contains("args"));
        assert!(!lower_data.contains("arguments"));
        assert!(!lower_data.contains("account"));
        assert!(!lower_data.contains("command"));
        assert!(!lower_data.contains("jvm"));
        assert!(!lower_data.contains("token"));
        assert!(!lower_data.contains("username"));
    }

    #[test]
    fn benchmark_suite_plans_are_deterministic_bounded_and_use_matrix_ids() {
        let matrix = benchmark_matrix();
        let profile_ids = matrix
            .profiles
            .iter()
            .map(|profile| profile.id)
            .collect::<HashSet<_>>();
        let run_type_ids = matrix
            .run_types
            .iter()
            .map(|run_type| run_type.id)
            .collect::<HashSet<_>>();
        let target_id_set = matrix
            .representative_targets
            .iter()
            .map(|target| target.id)
            .collect::<HashSet<_>>();
        for target in &matrix.representative_targets {
            assert!(profile_ids.contains(target.profile));
            assert!(run_type_ids.contains(target.run_type));
        }

        assert_eq!(
            benchmark_suite_plan("development").expect("development plan"),
            vec![
                BenchmarkSuiteRunSpec {
                    profile: "vanilla_baseline",
                    run_type: "coldish",
                    target_id: None,
                },
                BenchmarkSuiteRunSpec {
                    profile: "managed_default",
                    run_type: "repeat",
                    target_id: None,
                },
            ]
        );
        assert_eq!(
            benchmark_suite_plan("qualification")
                .expect("qualification plan")
                .len(),
            12
        );
        assert_eq!(
            benchmark_suite_plan("release_validation").expect("release plan"),
            vec![
                BenchmarkSuiteRunSpec {
                    profile: "vanilla_baseline",
                    run_type: "coldish",
                    target_id: Some("family_c_forge_1_12_2_vanilla_baseline"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "managed_default",
                    run_type: "coldish",
                    target_id: Some("family_c_forge_1_12_2_family_c_forge_core"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "legacy_family",
                    run_type: "coldish",
                    target_id: Some("legacy_1_8_9_forge_pvp"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "degraded_managed_path",
                    run_type: "coldish",
                    target_id: Some("degraded_managed_path"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "repeat_launch",
                    run_type: "repeat",
                    target_id: Some("repeat_managed_launch"),
                },
            ]
        );

        for mode in ["development", "qualification", "release_validation"] {
            let plan = benchmark_suite_plan(mode).expect("suite plan");
            assert!(!plan.is_empty());
            assert!(plan.len() <= 16);
            for run in plan {
                assert!(profile_ids.contains(run.profile));
                assert!(run_type_ids.contains(run.run_type));
                if let Some(target_id) = run.target_id {
                    assert!(target_id_set.contains(target_id));
                }
            }
        }
        assert_eq!(benchmark_suite_plan("nightly-check"), None);
    }

    #[test]
    fn benchmark_matrix_distinguishes_family_c_forge_baseline_and_managed_core() {
        let matrix = benchmark_matrix();
        let baseline = matrix
            .representative_targets
            .iter()
            .find(|target| target.id == "family_c_forge_1_12_2_vanilla_baseline")
            .expect("Family C Forge baseline target");
        let managed = matrix
            .representative_targets
            .iter()
            .find(|target| target.id == "family_c_forge_1_12_2_family_c_forge_core")
            .expect("Family C Forge managed core target");

        assert_eq!(baseline.family, "C");
        assert_eq!(managed.family, "C");
        assert_eq!(baseline.version, "1.12.2");
        assert_eq!(managed.version, baseline.version);
        assert_eq!(baseline.loader, "Forge");
        assert_eq!(managed.loader, baseline.loader);
        assert_eq!(baseline.profile, "vanilla_baseline");
        assert_eq!(managed.profile, "managed_default");
        assert_eq!(baseline.run_type, "coldish");
        assert_eq!(managed.run_type, "coldish");
        assert!(baseline.description.contains("baseline"));
        assert!(managed.description.contains("family-c-forge-core"));
        assert!(managed.intended_use.contains("Family C Forge core"));
    }
}
