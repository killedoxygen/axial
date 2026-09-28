//! Retained benchmark matrix, durable suites and user-controlled drivers.

use crate::{
    instances::{directory::Registry, model::InstanceId},
    launch::{
        coordinator::{LaunchCoordinator, LaunchIntentStatus, LaunchRequest},
        reports::{LaunchProofScenario, LaunchReportStore},
        session::{SessionManager, SessionPhase, SessionSnapshot},
    },
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, Connection, OptionalExtension, Transaction, params},
    },
    tasks::{CancellationToken, TaskOwner},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[path = "benchmarks_tests.rs"]
mod import_tests;

const MATRIX_SCHEMA: &str = "axial.launch.benchmark.matrix";
const MATRIX_SCHEMA_VERSION: u32 = 1;
const MAX_MATRIX_JSON_BYTES: usize = 12 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BenchmarkMatrix {
    pub schema: &'static str,
    pub schema_version: u32,
    pub modes: Vec<BenchmarkModeDescriptor>,
    pub run_types: Vec<BenchmarkRunTypeDescriptor>,
    pub profiles: Vec<BenchmarkProfileDescriptor>,
    pub representative_targets: Vec<BenchmarkTargetDescriptor>,
    pub limits: BenchmarkMatrixLimits,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BenchmarkModeDescriptor {
    pub id: &'static str,
    pub description: &'static str,
    pub intended_use: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BenchmarkRunTypeDescriptor {
    pub id: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BenchmarkProfileDescriptor {
    pub id: &'static str,
    pub scenario: &'static str,
    pub description: &'static str,
    pub intended_use: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BenchmarkTargetDescriptor {
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
pub struct BenchmarkMatrixLimits {
    pub max_payload_bytes: usize,
    pub custom_post_values_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchmarkSuiteRunSpec {
    pub profile: &'static str,
    pub run_type: &'static str,
    pub target_id: Option<&'static str>,
}

pub fn benchmark_matrix() -> BenchmarkMatrix {
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
                description: "Subsequent launch of the same target to isolate cache and managed reuse benefits.",
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
                intended_use: "Measure repeat-run cache and managed reuse benefits.",
            },
        ],
        representative_targets: vec![
            BenchmarkTargetDescriptor {
                id: "family_a_1_5_2_vanilla_enhanced",
                family: "A",
                version: "1.5.2",
                loader: "Vanilla",
                profile: "managed_default",
                run_type: "coldish",
                description: "Family A vanilla-enhanced target for older asset layout behavior.",
                intended_use: "Keep pre-1.6 coverage represented with the safest managed path.",
            },
            BenchmarkTargetDescriptor {
                id: "family_b_1_7_10_vanilla_enhanced",
                family: "B",
                version: "1.7.10",
                loader: "Vanilla",
                profile: "managed_default",
                run_type: "coldish",
                description: "Family B vanilla-enhanced target for late legacy launch behavior.",
                intended_use: "Keep 1.6-1.7.10 coverage represented without forcing a modern loader.",
            },
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
                id: "family_d_1_15_2_vanilla_enhanced",
                family: "D",
                version: "1.15.2",
                loader: "Vanilla",
                profile: "managed_default",
                run_type: "coldish",
                description: "Family D transitional vanilla-enhanced target.",
                intended_use: "Keep 1.13-1.15.2 coverage represented until a fuller transitional bundle is promoted.",
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
                intended_use: "Measure repeat-run cache and managed reuse effects.",
            },
        ],
        limits: BenchmarkMatrixLimits {
            max_payload_bytes: MAX_MATRIX_JSON_BYTES,
            custom_post_values_allowed: true,
        },
    }
}

pub fn benchmark_suite_plan(mode: &str) -> Option<Vec<BenchmarkSuiteRunSpec>> {
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
            BenchmarkSuiteRunSpec {
                profile: "managed_default",
                run_type: "coldish",
                target_id: Some("family_a_1_5_2_vanilla_enhanced"),
            },
            BenchmarkSuiteRunSpec {
                profile: "managed_default",
                run_type: "coldish",
                target_id: Some("family_b_1_7_10_vanilla_enhanced"),
            },
            BenchmarkSuiteRunSpec {
                profile: "managed_default",
                run_type: "coldish",
                target_id: Some("family_d_1_15_2_vanilla_enhanced"),
            },
        ]),
        _ => None,
    }
}

pub fn benchmark_suite_run_id(mode: &str, run_index: usize, run: BenchmarkSuiteRunSpec) -> String {
    let identity = format!(
        "{mode}|{run_index}|{}|{}|{}",
        run.profile,
        run.run_type,
        run.target_id.unwrap_or_default()
    );
    bounded_descriptor_token(&identity, "benchmark")
}

pub fn benchmark_suite_run_descriptor(
    mode: &str,
    run_index: usize,
    run: BenchmarkSuiteRunSpec,
) -> serde_json::Value {
    serde_json::json!({
        "run_index": run_index,
        "profile": run.profile,
        "run_type": run.run_type,
        "target_id": run.target_id,
        "benchmark_id": benchmark_suite_run_id(mode, run_index, run),
    })
}

pub fn benchmark_suite_manifest_run_inputs(
    mode: &str,
    plan: &[BenchmarkSuiteRunSpec],
) -> Vec<BenchmarkSuiteRunInput> {
    plan.iter()
        .enumerate()
        .map(|(index, run)| BenchmarkSuiteRunInput {
            run_index: index,
            profile: run.profile.to_string(),
            run_type: run.run_type.to_string(),
            target_id: run.target_id.map(str::to_string),
            benchmark_id: benchmark_suite_run_id(mode, index, *run),
        })
        .collect()
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
                "family_a_1_5_2_vanilla_enhanced",
                "family_b_1_7_10_vanilla_enhanced",
                "family_e_fabric_1_16_5_managed",
                "family_e_fabric_1_20_1_managed",
                "family_f_modern_fabric_managed",
                "family_c_forge_1_12_2_vanilla_baseline",
                "family_c_forge_1_12_2_family_c_forge_core",
                "family_d_1_15_2_vanilla_enhanced",
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
                BenchmarkSuiteRunSpec {
                    profile: "managed_default",
                    run_type: "coldish",
                    target_id: Some("family_a_1_5_2_vanilla_enhanced"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "managed_default",
                    run_type: "coldish",
                    target_id: Some("family_b_1_7_10_vanilla_enhanced"),
                },
                BenchmarkSuiteRunSpec {
                    profile: "managed_default",
                    run_type: "coldish",
                    target_id: Some("family_d_1_15_2_vanilla_enhanced"),
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
    fn benchmark_suite_run_ids_are_opaque_bounded_stable_and_unique() {
        let mut ids = HashSet::new();
        for mode in ["development", "qualification", "release_validation"] {
            let plan = benchmark_suite_plan(mode).expect("suite plan");
            for (run_index, run) in plan.iter().copied().enumerate() {
                let first = benchmark_suite_run_id(mode, run_index, run);
                let second = benchmark_suite_run_id(mode, run_index, run);

                assert_eq!(first, second);
                assert!(first.starts_with("benchmark-"));
                assert!(first.chars().count() < 48);
                assert!(!first.contains(mode));
                assert!(!first.contains(run.profile));
                assert!(!first.contains(run.run_type));
                if let Some(target_id) = run.target_id {
                    assert!(!first.contains(target_id));
                }
                assert!(valid_token(&first));
                assert!(ids.insert(first), "benchmark run id collision");
            }
        }

        let run = BenchmarkSuiteRunSpec {
            profile: "managed_default",
            run_type: "coldish",
            target_id: Some("family_c_forge_1_12_2_family_c_forge_core"),
        };
        let base = benchmark_suite_run_id("release_validation", 1, run);
        assert_ne!(base, benchmark_suite_run_id("development", 1, run));
        assert_ne!(base, benchmark_suite_run_id("release_validation", 2, run));
        assert_ne!(
            base,
            benchmark_suite_run_id(
                "release_validation",
                1,
                BenchmarkSuiteRunSpec {
                    profile: "vanilla_baseline",
                    ..run
                },
            )
        );
        assert_ne!(
            base,
            benchmark_suite_run_id(
                "release_validation",
                1,
                BenchmarkSuiteRunSpec {
                    run_type: "repeat",
                    ..run
                },
            )
        );
        assert_ne!(
            base,
            benchmark_suite_run_id(
                "release_validation",
                1,
                BenchmarkSuiteRunSpec {
                    target_id: Some("family_c_forge_1_12_2_vanilla_baseline"),
                    ..run
                },
            )
        );
    }

    #[test]
    fn custom_benchmark_descriptors_remain_allowed_without_arbitrary_modes() {
        assert!(validate_descriptor("shader_pack_lab", "warm_cache_3", "development").is_ok());
        assert!(validate_descriptor("managed_default", "repeat", "invented_mode").is_err());
        assert!(validate_descriptor("../private", "repeat", "development").is_err());
        assert!(validate_descriptor("managed_default", "", "development").is_err());
        assert!(validate_descriptor(&"a".repeat(97), "repeat", "development").is_err());
    }

    #[test]
    fn benchmark_matrix_has_representative_target_for_each_version_family() {
        let families = benchmark_matrix()
            .representative_targets
            .iter()
            .map(|target| target.family)
            .collect::<HashSet<_>>();

        for family in ["A", "B", "C", "D", "E", "F"] {
            assert!(families.contains(family), "missing family {family} target");
        }
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

/// Stable redacted descriptor identity, compatible with predecessor manifests.
pub fn bounded_descriptor_token(value: &str, prefix: &str) -> String {
    let value = value.trim();
    if valid_token(value) {
        return value.to_owned();
    }
    let hash = value.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{prefix}-{hash:016x}")
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchmarkSuiteRunInput {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: Option<String>,
    pub benchmark_id: String,
}

pub const MIGRATION: Migration = Migration {
    id: "performance_benchmarks.v1",
    sql: "CREATE TABLE benchmark_suites (suite_id TEXT PRIMARY KEY NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=262144)) STRICT;
    CREATE TABLE benchmark_drivers (driver_id TEXT PRIMARY KEY NOT NULL, payload BLOB NOT NULL CHECK(length(payload)<=16384), request BLOB CHECK(length(request)<=8192)) STRICT;",
};

pub const MIGRATION_V2: Migration = Migration {
    id: "performance_benchmarks.v2",
    sql: "ALTER TABLE benchmark_suites ADD COLUMN source_suite_id TEXT;
    CREATE UNIQUE INDEX benchmark_suites_source ON benchmark_suites(source_suite_id) WHERE source_suite_id IS NOT NULL;
    ALTER TABLE benchmark_drivers ADD COLUMN source_driver_id TEXT;
    CREATE UNIQUE INDEX benchmark_drivers_source ON benchmark_drivers(source_driver_id) WHERE source_driver_id IS NOT NULL;",
};

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("benchmark input is invalid")]
    Invalid,
    #[error("benchmark record was not found")]
    NotFound,
    #[error("benchmark work is already active or requires settlement")]
    Busy,
    #[error("benchmark suite is complete")]
    Complete,
    #[error("benchmark launch failed")]
    LaunchFailed,
    #[error("benchmark evidence or storage is unavailable")]
    Unavailable,
    #[error("imported benchmark history conflicts with retained metadata")]
    ConflictingHistory,
    #[error("benchmark storage is unavailable")]
    Storage(#[from] StorageError),
}
impl From<rusqlite::Error> for BenchmarkError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifest {
    pub schema: String,
    pub schema_version: u32,
    pub suite_id: String,
    pub instance_id: String,
    pub mode: String,
    pub created_at: String,
    pub updated_at: String,
    pub runs: Vec<BenchmarkSuiteManifestRun>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub historical: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteManifestRun {
    pub run_index: usize,
    pub profile: String,
    pub run_type: String,
    pub target_id: String,
    pub benchmark_id: String,
    pub session_id: Option<String>,
    pub launched_at: Option<String>,
    pub state: String,
    /// Persisted before accepting launch; a response retry retains one launch intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_intent: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkLaunchRequest {
    pub instance_id: Option<InstanceId>,
    pub username: Option<String>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub client_started_at_ms: Option<i64>,
    pub profile: Option<String>,
    pub run_type: Option<String>,
    pub benchmark_mode: Option<String>,
    pub suite_mode: Option<String>,
    pub suite_id: Option<String>,
    pub run_index: Option<usize>,
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuiteDriverStatus {
    pub id: String,
    pub suite_id: String,
    pub mode: String,
    pub state: String,
    pub interval_ms: u64,
    pub run_count: usize,
    pub launched_run_count: usize,
    pub pending_run_index: Option<usize>,
    pub active_session_id: Option<String>,
    pub last_run_index: Option<usize>,
    pub last_session_id: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub historical: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn require_mutable(historical: bool) -> Result<(), BenchmarkError> {
    if historical {
        Err(BenchmarkError::Invalid)
    } else {
        Ok(())
    }
}

fn has_queued_handoff(driver: &BenchmarkSuiteDriverStatus) -> bool {
    driver.historical
        && driver.error.as_deref() == Some("driver automatic resume queued after restart")
}

/// Historical metadata only. Preparation is outside the instance publication
/// transaction; insertion grants neither launch intent nor driver authority.
#[derive(Clone)]
pub(crate) struct PreparedBenchmarkImport {
    suites: Vec<(BenchmarkSuiteManifest, Vec<u8>)>,
    drivers: Vec<(BenchmarkSuiteDriverStatus, Vec<u8>)>,
}

impl PreparedBenchmarkImport {
    pub(crate) fn prepare(
        suites: Vec<BenchmarkSuiteManifest>,
        drivers: Vec<BenchmarkSuiteDriverStatus>,
    ) -> Result<Self, BenchmarkError> {
        if suites.len().saturating_add(drivers.len()) > 1024 {
            return Err(BenchmarkError::Invalid);
        }
        let mut suite_modes = BTreeMap::new();
        let mut sessions = BTreeSet::new();
        for suite in &suites {
            if !suite.historical
                || validate_suite(suite).is_err()
                || suite_modes
                    .insert(suite.suite_id.as_str(), suite.mode.as_str())
                    .is_some()
                || suite
                    .runs
                    .iter()
                    .filter_map(|run| run.session_id.as_deref())
                    .any(|session| !sessions.insert(session))
            {
                return Err(BenchmarkError::Invalid);
            }
        }
        let mut driver_ids = BTreeSet::new();
        for driver in &drivers {
            if !driver.historical
                || validate_driver(driver).is_err()
                || !driver_ids.insert(driver.id.as_str())
                || suite_modes.get(driver.suite_id.as_str()).copied() != Some(driver.mode.as_str())
            {
                return Err(BenchmarkError::Invalid);
            }
        }
        let mut bytes = 0usize;
        let suites = suites
            .into_iter()
            .map(|suite| {
                let encoded = serde_json::to_vec(&suite).map_err(|_| BenchmarkError::Invalid)?;
                bytes = bytes.saturating_add(encoded.len());
                if encoded.len() > 256 * 1024 || bytes > 64 * 1024 * 1024 {
                    return Err(BenchmarkError::Invalid);
                }
                Ok((suite, encoded))
            })
            .collect::<Result<_, BenchmarkError>>()?;
        let drivers = drivers
            .into_iter()
            .map(|driver| {
                let encoded = serde_json::to_vec(&driver).map_err(|_| BenchmarkError::Invalid)?;
                bytes = bytes.saturating_add(encoded.len());
                if encoded.len() > 16 * 1024 || bytes > 64 * 1024 * 1024 {
                    return Err(BenchmarkError::Invalid);
                }
                Ok((driver, encoded))
            })
            .collect::<Result<_, BenchmarkError>>()?;
        Ok(Self { suites, drivers })
    }

    pub(crate) fn insert_in(&self, tx: &Transaction<'_>) -> Result<(), BenchmarkError> {
        for (suite, bytes) in &self.suites {
            match stored_suite(tx, &suite.suite_id)? {
                Some(saved) if saved == *suite => {}
                Some(_) => return Err(BenchmarkError::ConflictingHistory),
                None => {
                    tx.execute(
                        "INSERT INTO benchmark_suites(suite_id,payload) VALUES(?1,?2)",
                        params![suite.suite_id, bytes],
                    )?;
                }
            }
        }
        for (driver, bytes) in &self.drivers {
            match stored_driver(tx, &driver.id)? {
                Some(saved) if saved == *driver => {}
                Some(_) => return Err(BenchmarkError::ConflictingHistory),
                None => {
                    tx.execute("INSERT INTO benchmark_drivers(driver_id,payload,request) VALUES(?1,?2,NULL)", params![driver.id, bytes])?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn verify_in(&self, tx: &Transaction<'_>) -> Result<(), BenchmarkError> {
        for (suite, _) in &self.suites {
            if stored_suite(tx, &suite.suite_id)?.as_ref() != Some(suite) {
                return Err(BenchmarkError::ConflictingHistory);
            }
        }
        for (driver, _) in &self.drivers {
            if stored_driver(tx, &driver.id)?.as_ref() != Some(driver) {
                return Err(BenchmarkError::ConflictingHistory);
            }
        }
        Ok(())
    }
}

fn stored_suite(
    connection: &Connection,
    id: &str,
) -> Result<Option<BenchmarkSuiteManifest>, BenchmarkError> {
    read_suite(connection, id)?
        .map(|(suite, source)| {
            validate_suite_in(connection, &suite, source.as_deref())?;
            Ok(suite)
        })
        .transpose()
}

fn read_suite(
    connection: &Connection,
    id: &str,
) -> Result<Option<(BenchmarkSuiteManifest, Option<String>)>, BenchmarkError> {
    let record: Option<(Vec<u8>, Option<String>)> = connection
        .query_row(
            "SELECT payload,source_suite_id FROM benchmark_suites WHERE suite_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    record
        .map(|(bytes, source)| {
            let suite: BenchmarkSuiteManifest =
                serde_json::from_slice(&bytes).map_err(|_| BenchmarkError::Unavailable)?;
            if suite.suite_id != id {
                return Err(BenchmarkError::Unavailable);
            }
            Ok((suite, source))
        })
        .transpose()
}

fn current_plan_matches(suite: &BenchmarkSuiteManifest) -> bool {
    benchmark_suite_plan(&suite.mode).is_some_and(|plan| {
        suite.runs.len() == plan.len()
            && suite
                .runs
                .iter()
                .zip(plan)
                .enumerate()
                .all(|(index, (run, expected))| {
                    run.run_index == index
                        && run.profile == expected.profile
                        && run.run_type == expected.run_type
                        && run.target_id == expected.target_id.unwrap_or("")
                        && run.benchmark_id == benchmark_suite_run_id(&suite.mode, index, expected)
                })
    })
}

fn validate_source_suite(
    connection: &Connection,
    source: &BenchmarkSuiteManifest,
) -> Result<(), BenchmarkError> {
    if !source.historical || !current_plan_matches(source) {
        return Err(BenchmarkError::Invalid);
    }
    validate_historical_suite(source)?;
    for run in &source.runs {
        let Some(session) = run.session_id.as_deref() else {
            continue;
        };
        let proof = LaunchReportStore::get_in(connection, session)
            .map_err(|_| BenchmarkError::Unavailable)?
            .ok_or(BenchmarkError::Unavailable)?;
        if proof.instance_id != source.instance_id
            || proof.scenario.benchmark_id.as_deref() != Some(run.benchmark_id.as_str())
            || proof.scenario.benchmark_profile.as_deref() != Some(run.profile.as_str())
            || proof.scenario.benchmark_run_type.as_deref() != Some(run.run_type.as_str())
            || proof.scenario.benchmark_mode.as_deref() != Some(source.mode.as_str())
            || historical_time(&proof.launched_at)?
                != historical_time(
                    run.launched_at
                        .as_deref()
                        .ok_or(BenchmarkError::Unavailable)?,
                )?
            || !proof.matches_imported_terminal_state(&run.state)
        {
            return Err(BenchmarkError::Unavailable);
        }
    }
    Ok(())
}

fn validate_suite_in(
    connection: &Connection,
    suite: &BenchmarkSuiteManifest,
    source_id: Option<&str>,
) -> Result<(), BenchmarkError> {
    let Some(source_id) = source_id else {
        return validate_suite(suite);
    };
    let (source, parent) = read_suite(connection, source_id)?.ok_or(BenchmarkError::Unavailable)?;
    if suite.historical
        || parent.is_some()
        || suite.instance_id != source.instance_id
        || suite.mode != source.mode
    {
        return Err(BenchmarkError::Unavailable);
    }
    validate_source_suite(connection, &source)?;
    validate_live_suite(suite, Some(&source))
}

fn stored_driver(
    connection: &Connection,
    id: &str,
) -> Result<Option<BenchmarkSuiteDriverStatus>, BenchmarkError> {
    let record: Option<(Vec<u8>, Option<Vec<u8>>, Option<String>)> = connection
        .query_row(
            "SELECT payload,request,source_driver_id FROM benchmark_drivers WHERE driver_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    record
        .map(|(bytes, request, source)| {
            let driver = decode_driver(id, &bytes, request.as_deref())?;
            validate_driver_link(
                connection,
                &driver,
                request.as_deref(),
                source.as_deref(),
                &mut BTreeMap::new(),
            )?;
            Ok(driver)
        })
        .transpose()
}

fn validate_driver_link(
    connection: &Connection,
    driver: &BenchmarkSuiteDriverStatus,
    request: Option<&[u8]>,
    source_id: Option<&str>,
    suites: &mut BTreeMap<String, (BenchmarkSuiteManifest, Option<String>)>,
) -> Result<(), BenchmarkError> {
    let Some(source_id) = source_id else {
        return Ok(());
    };
    if driver.historical {
        return Err(BenchmarkError::Unavailable);
    }
    let (bytes, source_request, parent): (Vec<u8>, Option<Vec<u8>>, Option<String>) = connection
        .query_row(
            "SELECT payload,request,source_driver_id FROM benchmark_drivers WHERE driver_id=?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or(BenchmarkError::Unavailable)?;
    let source = decode_driver(source_id, &bytes, source_request.as_deref())?;
    if !source.historical || has_queued_handoff(&source) || parent.is_some() {
        return Err(BenchmarkError::Unavailable);
    }
    if !suites.contains_key(&driver.suite_id) {
        let record =
            read_suite(connection, &driver.suite_id)?.ok_or(BenchmarkError::Unavailable)?;
        validate_suite_in(connection, &record.0, record.1.as_deref())?;
        suites.insert(driver.suite_id.clone(), record);
    }
    let (suite, source_suite) = &suites[&driver.suite_id];
    let input: BenchmarkLaunchRequest =
        serde_json::from_slice(request.ok_or(BenchmarkError::Unavailable)?)
            .map_err(|_| BenchmarkError::Unavailable)?;
    if source_suite.as_deref() != Some(source.suite_id.as_str())
        || source.mode != suite.mode
        || driver.mode != suite.mode
        || driver.run_count != suite.runs.len()
        || driver.interval_ms != source.interval_ms
        || input.instance_id.as_ref().map(InstanceId::as_str) != Some(suite.instance_id.as_str())
        || input.suite_id.as_deref() != Some(suite.suite_id.as_str())
        || input.suite_mode.as_deref() != Some(suite.mode.as_str())
        || input.interval_ms != Some(source.interval_ms)
        || input.username.is_some()
        || input.max_memory_mb.is_some()
        || input.min_memory_mb.is_some()
        || input.client_started_at_ms.is_some()
        || input.profile.is_some()
        || input.run_type.is_some()
        || input.benchmark_mode.is_some()
        || input.run_index.is_some()
    {
        return Err(BenchmarkError::Unavailable);
    }
    Ok(())
}

fn resumed_driver_in(
    connection: &Connection,
    source_id: &str,
) -> Result<Option<BenchmarkSuiteDriverStatus>, BenchmarkError> {
    let id: Option<String> = connection
        .query_row(
            "SELECT driver_id FROM benchmark_drivers WHERE source_driver_id=?1",
            [source_id],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| stored_driver(connection, &id)?.ok_or(BenchmarkError::Unavailable))
        .transpose()
}

fn continuation_in(
    connection: &Connection,
    id: &str,
) -> Result<
    (
        BenchmarkSuiteDriverStatus,
        BenchmarkSuiteManifest,
        Option<BenchmarkSuiteManifest>,
    ),
    BenchmarkError,
> {
    let driver = stored_driver(connection, id)?.ok_or(BenchmarkError::NotFound)?;
    if !driver.historical || has_queued_handoff(&driver) {
        return Err(BenchmarkError::Invalid);
    }
    let (source, parent) =
        read_suite(connection, &driver.suite_id)?.ok_or(BenchmarkError::NotFound)?;
    if parent.is_some() || driver.mode != source.mode {
        return Err(BenchmarkError::Unavailable);
    }
    validate_source_suite(connection, &source)?;
    let successor: Option<String> = connection
        .query_row(
            "SELECT suite_id FROM benchmark_suites WHERE source_suite_id=?1",
            [&source.suite_id],
            |row| row.get(0),
        )
        .optional()?;
    let successor = successor
        .map(|id| stored_suite(connection, &id)?.ok_or(BenchmarkError::Unavailable))
        .transpose()?;
    if !successor
        .as_ref()
        .unwrap_or(&source)
        .runs
        .iter()
        .any(|run| run.state == "pending")
    {
        return Err(BenchmarkError::Complete);
    }
    Ok((driver, source, successor))
}

fn stored_drivers_in(
    connection: &Connection,
) -> Result<Vec<BenchmarkSuiteDriverStatus>, BenchmarkError> {
    let mut query = connection.prepare("SELECT driver_id,payload,request,source_driver_id FROM benchmark_drivers ORDER BY rowid DESC LIMIT 4097")?;
    let mut rows = query.query([])?;
    let mut drivers = Vec::new();
    let mut suites = BTreeMap::new();
    while let Some(row) = rows.next()? {
        if drivers.len() == 4096 {
            return Err(BenchmarkError::Unavailable);
        }
        let id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let request: Option<Vec<u8>> = row.get(2)?;
        let source: Option<String> = row.get(3)?;
        let driver = decode_driver(&id, &bytes, request.as_deref())?;
        validate_driver_link(
            connection,
            &driver,
            request.as_deref(),
            source.as_deref(),
            &mut suites,
        )?;
        drivers.push(driver);
    }
    Ok(drivers)
}

fn decode_driver(
    id: &str,
    bytes: &[u8],
    request: Option<&[u8]>,
) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
    let driver: BenchmarkSuiteDriverStatus =
        serde_json::from_slice(bytes).map_err(|_| BenchmarkError::Unavailable)?;
    validate_driver(&driver)?;
    if driver.id != id || (driver.historical && request.is_some()) {
        return Err(BenchmarkError::Unavailable);
    }
    Ok(driver)
}

#[derive(Clone)]
pub struct BenchmarkService {
    storage: Arc<MetadataStore>,
    registry: Registry,
    reports: Arc<LaunchReportStore>,
    launches: LaunchCoordinator,
    sessions: SessionManager,
    tasks: TaskOwner,
    active_drivers: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
    gates: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl BenchmarkService {
    pub fn new(
        storage: Arc<MetadataStore>,
        registry: Registry,
        reports: Arc<LaunchReportStore>,
        launches: LaunchCoordinator,
        sessions: SessionManager,
        tasks: TaskOwner,
    ) -> Result<Self, BenchmarkError> {
        let service = Self {
            storage,
            registry,
            reports,
            launches,
            sessions,
            tasks,
            active_drivers: Arc::new(Mutex::new(BTreeMap::new())),
            gates: Arc::new(Mutex::new(BTreeMap::new())),
        };
        // The existing driver resumes only after tick reconciles each durable
        // session mapping; an uncertain accepted process remains interrupted.
        for mut driver in service.stored_drivers()? {
            if !driver.historical && matches!(driver.state.as_str(), "running" | "waiting") {
                driver.state = "interrupted".into();
                driver.error = Some("Driver interrupted by application restart".into());
                service.save_driver(&driver)?;
            }
        }
        Ok(service)
    }

    pub fn reports(&self) -> &LaunchReportStore {
        &self.reports
    }

    /// Resume only user-created drivers which were running at this startup.
    /// A run with an uncertain launch mapping remains fenced by `tick`.
    pub fn resume_interrupted_drivers(&self) -> Result<usize, BenchmarkError> {
        let mut resumed = 0;
        for driver in self
            .stored_drivers()?
            .into_iter()
            .filter(|driver| {
                !driver.historical
                    && driver.state == "interrupted"
                    && driver.error.as_deref() == Some("Driver interrupted by application restart")
            })
            .take(8)
        {
            self.resume_driver(&driver.id)?;
            resumed += 1;
        }
        Ok(resumed)
    }

    pub async fn qualification(
        &self,
        suite_id: &str,
        performance: &super::PerformanceService,
    ) -> Result<serde_json::Value, BenchmarkError> {
        let suite = self.suite(suite_id)?.ok_or(BenchmarkError::NotFound)?;
        let mut proofs = Vec::new();
        for run in &suite.runs {
            if let Some(session_id) = &run.session_id {
                if let Some(proof) = self
                    .reports
                    .get(session_id)
                    .map_err(|_| BenchmarkError::Unavailable)?
                {
                    proofs.push(proof);
                }
            }
        }
        let instance_id = suite
            .instance_id
            .parse()
            .map_err(|_| BenchmarkError::Unavailable)?;
        let inspected = performance.inspect(&instance_id).await.ok();
        let managed = inspected.as_ref().and_then(|inspection| {
            inspection.state.as_ref().map(|state| {
                super::qualification::managed_install_evidence(state, inspection.health)
            })
        });
        Ok(super::qualification::qualification_payload(
            &suite,
            &proofs,
            managed.as_ref(),
            true,
        ))
    }

    fn launch_request(
        &self,
        input: &BenchmarkLaunchRequest,
    ) -> Result<LaunchRequest, BenchmarkError> {
        let instance_id = input
            .instance_id
            .clone()
            .or(self
                .registry
                .last_instance_id()
                .map_err(|_| BenchmarkError::Unavailable)?)
            .ok_or(BenchmarkError::Invalid)?;
        self.registry
            .get_live(&instance_id)
            .map_err(|_| BenchmarkError::NotFound)?;
        let request = LaunchRequest {
            instance_id,
            version_id: None,
            username: input.username.clone(),
            max_memory_mb: input.max_memory_mb,
            min_memory_mb: input.min_memory_mb,
            client_started_at_ms: input.client_started_at_ms,
            intent_key: None,
        };
        request.validate().map_err(|_| BenchmarkError::Invalid)?;
        Ok(request)
    }

    pub async fn launch(
        &self,
        input: BenchmarkLaunchRequest,
    ) -> Result<SessionSnapshot, BenchmarkError> {
        if input.suite_mode.is_some() || input.suite_id.is_some() || input.run_index.is_some() {
            return Err(BenchmarkError::Invalid);
        }
        let request = self.launch_request(&input)?;
        let profile = input.profile.as_deref().unwrap_or("managed_default").trim();
        let run_type = input.run_type.as_deref().unwrap_or("coldish").trim();
        let mode = input
            .benchmark_mode
            .as_deref()
            .unwrap_or("development")
            .trim();
        validate_descriptor(profile, run_type, mode)?;
        let id = format!("benchmark-{}", uuid::Uuid::new_v4().simple());
        self.launch_run(request, profile, run_type, mode, &id).await
    }

    async fn launch_run(
        &self,
        request: LaunchRequest,
        profile: &str,
        run_type: &str,
        mode: &str,
        id: &str,
    ) -> Result<SessionSnapshot, BenchmarkError> {
        self.launches
            .launch_benchmark_configured(request, benchmark_scenario(profile, run_type, mode, id))
            .await
            .map_err(|_| BenchmarkError::LaunchFailed)
    }

    pub fn suite(&self, id: &str) -> Result<Option<BenchmarkSuiteManifest>, BenchmarkError> {
        if !valid_token(id) {
            return Err(BenchmarkError::Invalid);
        }
        self.storage.read(|connection| stored_suite(connection, id))
    }

    pub fn ensure_suite(
        &self,
        input: &BenchmarkLaunchRequest,
    ) -> Result<BenchmarkSuiteManifest, BenchmarkError> {
        if input.benchmark_mode.is_some() {
            return Err(BenchmarkError::Invalid);
        }
        if let Some(existing) = input
            .suite_id
            .as_deref()
            .map(|id| self.suite(id))
            .transpose()?
            .flatten()
        {
            require_mutable(existing.historical)?;
            if input
                .instance_id
                .as_ref()
                .is_some_and(|id| id.as_str() != existing.instance_id)
                || input
                    .suite_mode
                    .as_deref()
                    .is_some_and(|mode| mode != existing.mode)
            {
                return Err(BenchmarkError::Invalid);
            }
            return Ok(existing);
        }
        let request = self.launch_request(input)?;
        let mode = input.suite_mode.as_deref().unwrap_or("development");
        let plan = benchmark_suite_plan(mode).ok_or(BenchmarkError::Invalid)?;
        let id = input.suite_id.clone().unwrap_or_else(|| {
            format!(
                "suite-{}",
                bounded_descriptor_token(&format!("{}:{}", request.instance_id, mode), "id")
            )
        });
        if id.starts_with("legacy-") {
            return Err(BenchmarkError::Invalid);
        }
        if let Some(existing) = self.suite(&id)? {
            require_mutable(existing.historical)?;
            if existing.instance_id != request.instance_id.as_str() || existing.mode != mode {
                return Err(BenchmarkError::Invalid);
            }
            return Ok(existing);
        }
        let now = now();
        let suite = BenchmarkSuiteManifest {
            schema: "axial.launch.benchmark.suite".into(),
            schema_version: 2,
            suite_id: id,
            instance_id: request.instance_id.to_string(),
            mode: mode.into(),
            created_at: now.clone(),
            updated_at: now,
            historical: false,
            runs: plan
                .iter()
                .copied()
                .enumerate()
                .map(|(index, run)| BenchmarkSuiteManifestRun {
                    run_index: index,
                    profile: run.profile.into(),
                    run_type: run.run_type.into(),
                    target_id: run.target_id.unwrap_or("").into(),
                    benchmark_id: benchmark_suite_run_id(mode, index, run),
                    session_id: None,
                    launched_at: None,
                    state: "pending".into(),
                    launch_intent: Some(uuid::Uuid::new_v4().to_string()),
                })
                .collect(),
        };
        validate_suite(&suite)?;
        let bytes = serde_json::to_vec(&suite).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            tx.execute(
                "INSERT INTO benchmark_suites(suite_id,payload) VALUES(?1,?2)",
                params![suite.suite_id, bytes],
            )?;
            Ok::<_, BenchmarkError>(())
        })?;
        Ok(suite)
    }

    pub async fn tick(
        &self,
        input: BenchmarkLaunchRequest,
    ) -> Result<serde_json::Value, BenchmarkError> {
        let suite = self.ensure_suite(&input)?;
        let gate = self
            .gates
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(suite.suite_id.clone())
            .or_default()
            .clone();
        let _lease = gate.try_lock_owned().map_err(|_| BenchmarkError::Busy)?;
        let mut suite = self
            .suite(&suite.suite_id)?
            .ok_or(BenchmarkError::NotFound)?;
        require_mutable(suite.historical)?;
        let mut preparing_index = None;
        for index in 0..suite.runs.len() {
            // A linked terminal row was verified against immutable imported
            // evidence by `suite`; it has no live intent to reconcile or replace.
            if suite.runs[index].launch_intent.is_none() {
                continue;
            }
            if let Some(id) = suite.runs[index].session_id.clone() {
                if let Some(session) = self.sessions.snapshot_by_session_id(&id) {
                    if session.phase != SessionPhase::Exited {
                        return Ok(
                            serde_json::json!({"state":"active","suite":suite,"active_session_id":id}),
                        );
                    }
                }
                if let Some(proof) = self
                    .reports
                    .get(&id)
                    .map_err(|_| BenchmarkError::Unavailable)?
                {
                    if proof.instance_id != suite.instance_id
                        || proof.scenario.benchmark_id.as_deref()
                            != Some(suite.runs[index].benchmark_id.as_str())
                        || proof.scenario.benchmark_profile.as_deref()
                            != Some(suite.runs[index].profile.as_str())
                        || proof.scenario.benchmark_run_type.as_deref()
                            != Some(suite.runs[index].run_type.as_str())
                        || proof.scenario.benchmark_mode.as_deref() != Some(suite.mode.as_str())
                    {
                        return Err(BenchmarkError::Unavailable);
                    }
                    suite.runs[index].launched_at = Some(proof.launched_at);
                    suite.runs[index].state = proof.outcome;
                } else {
                    match self
                        .launches
                        .intent(
                            suite.runs[index]
                                .launch_intent
                                .as_deref()
                                .ok_or(BenchmarkError::Unavailable)?,
                        )
                        .map_err(|_| BenchmarkError::Unavailable)?
                    {
                        Some(LaunchIntentStatus::Preparing) => preparing_index = Some(index),
                        Some(LaunchIntentStatus::Rejected { .. }) => {
                            suite.runs[index].state = "failed".into()
                        }
                        Some(LaunchIntentStatus::Accepted { session })
                            if session.phase == SessionPhase::Exited
                                && session.session_id == id
                                && session.instance_id.as_str() == suite.instance_id =>
                        {
                            // Settled effects are not benchmark measurements.
                            // Keep this exact run blocked until its report exists.
                            return Err(BenchmarkError::Unavailable);
                        }
                        _ => {
                            suite.runs[index].state = "interrupted".into();
                            self.save_suite(&suite)?;
                            return Ok(
                                serde_json::json!({"state":"interrupted","suite":suite,"active_session_id":id}),
                            );
                        }
                    }
                }
            } else if matches!(
                suite.runs[index].state.as_str(),
                "launching" | "running" | "interrupted"
            ) {
                // Old manifests may have crashed before mapping their session.
                // Absence of a mapping cannot prove that no process was started.
                suite.runs[index].state = "interrupted".into();
                self.save_suite(&suite)?;
                return Ok(serde_json::json!({"state":"interrupted","suite":suite}));
            }
        }
        let index = match preparing_index.or(input.run_index) {
            Some(index) if index < suite.runs.len() => index,
            Some(_) => return Err(BenchmarkError::Invalid),
            None => match suite.runs.iter().position(|run| run.state == "pending") {
                Some(index) => index,
                None if suite.runs.iter().any(|run| run.state == "launching") => {
                    return Err(BenchmarkError::Busy);
                }
                None => {
                    self.save_suite(&suite)?;
                    return Ok(serde_json::json!({"state":"complete","suite":suite}));
                }
            },
        };
        if suite.runs[index].launch_intent.is_none() {
            return Err(BenchmarkError::Invalid);
        }
        if suite.runs[index].state == "launching" && preparing_index != Some(index) {
            return Err(BenchmarkError::Busy);
        }
        if input.run_index.is_some() && preparing_index != Some(index) {
            suite.runs[index].launch_intent = Some(uuid::Uuid::new_v4().to_string());
        }
        let mut captured = input.clone();
        captured.instance_id = Some(
            suite
                .instance_id
                .parse()
                .map_err(|_| BenchmarkError::Unavailable)?,
        );
        let run = &suite.runs[index];
        let scenario =
            benchmark_scenario(&run.profile, &run.run_type, &suite.mode, &run.benchmark_id);
        let mut request = if preparing_index == Some(index) {
            self.launches
                .benchmark_request(
                    run.launch_intent
                        .as_deref()
                        .ok_or(BenchmarkError::Unavailable)?,
                    run.session_id
                        .as_deref()
                        .ok_or(BenchmarkError::Unavailable)?,
                    captured
                        .instance_id
                        .as_ref()
                        .ok_or(BenchmarkError::Unavailable)?,
                    &scenario,
                )
                .map_err(|_| BenchmarkError::Unavailable)?
        } else {
            self.launch_request(&captured)?
        };
        request.intent_key = run.launch_intent.clone();
        let session_id = self
            .launches
            .reserve_benchmark(&request, &scenario)
            .map_err(|_| BenchmarkError::LaunchFailed)?;
        suite.runs[index].state = "launching".into();
        suite.runs[index].session_id = Some(session_id.clone());
        suite.runs[index].launched_at = None;
        self.save_suite(&suite)?;
        let run = suite.runs[index].clone();
        let launched = self
            .launch_run(
                request,
                &run.profile,
                &run.run_type,
                &suite.mode,
                &run.benchmark_id,
            )
            .await;
        match launched {
            Ok(session) => {
                if session.session_id != session_id {
                    return Err(BenchmarkError::Unavailable);
                }
                suite.runs[index].launched_at = Some(session.launched_at.clone());
                suite.runs[index].state = "running".into();
                self.save_suite(&suite)?;
                Ok(
                    serde_json::json!({"state":"launched","suite":suite,"session":session,"run_index":index}),
                )
            }
            Err(error) => {
                suite.runs[index].state = if matches!(
                    self.launches.intent(
                        run.launch_intent
                            .as_deref()
                            .ok_or(BenchmarkError::Unavailable)?
                    ),
                    Ok(Some(LaunchIntentStatus::Interrupted { .. }))
                ) {
                    "interrupted"
                } else {
                    "failed"
                }
                .into();
                self.save_suite(&suite)?;
                Err(error)
            }
        }
    }

    pub fn drivers(&self) -> Result<Vec<BenchmarkSuiteDriverStatus>, BenchmarkError> {
        Ok(self.stored_drivers()?.into_iter().take(64).collect())
    }

    fn stored_drivers(&self) -> Result<Vec<BenchmarkSuiteDriverStatus>, BenchmarkError> {
        self.storage.read(stored_drivers_in)
    }

    pub fn driver(&self, id: &str) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        if !valid_token(id) {
            return Err(BenchmarkError::Invalid);
        }
        self.storage
            .read(|connection| stored_driver(connection, id)?.ok_or(BenchmarkError::NotFound))
    }

    fn suite_driver_running(&self, suite_id: &str) -> Result<bool, BenchmarkError> {
        Ok(self.stored_drivers()?.iter().any(|driver| {
            driver.suite_id == suite_id && matches!(driver.state.as_str(), "running" | "waiting")
        }))
    }

    fn admit_suite(
        &self,
        suite_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, BenchmarkError> {
        self.gates
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(suite_id.to_owned())
            .or_default()
            .clone()
            .try_lock_owned()
            .map_err(|_| BenchmarkError::Busy)
    }

    pub async fn start_driver(
        &self,
        input: BenchmarkLaunchRequest,
    ) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        let suite = self.ensure_suite(&input)?;
        let lease = self.admit_suite(&suite.suite_id)?;
        if self.suite_driver_running(&suite.suite_id)? {
            return Err(BenchmarkError::Busy);
        }
        let now = now();
        let driver = BenchmarkSuiteDriverStatus {
            id: format!("benchmark-suite-driver-{}", uuid::Uuid::new_v4().simple()),
            suite_id: suite.suite_id.clone(),
            mode: suite.mode.clone(),
            state: "running".into(),
            interval_ms: input.interval_ms.unwrap_or(30_000).clamp(5_000, 3_600_000),
            run_count: suite.runs.len(),
            launched_run_count: suite
                .runs
                .iter()
                .filter(|run| run.session_id.is_some())
                .count(),
            pending_run_index: None,
            active_session_id: None,
            last_run_index: None,
            last_session_id: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
            historical: false,
        };
        let mut captured = input.clone();
        captured.instance_id = Some(
            suite
                .instance_id
                .parse()
                .map_err(|_| BenchmarkError::Unavailable)?,
        );
        captured.suite_id = Some(suite.suite_id);
        captured.suite_mode = Some(suite.mode);
        let request = serde_json::to_vec(&captured).map_err(|_| BenchmarkError::Unavailable)?;
        let payload = serde_json::to_vec(&driver).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            tx.execute(
                "INSERT INTO benchmark_drivers(driver_id,payload,request) VALUES(?1,?2,?3)",
                params![driver.id, payload, request],
            )?;
            Ok::<_, BenchmarkError>(())
        })?;
        drop(lease);
        self.run_driver(driver.clone(), captured)?;
        Ok(driver)
    }

    pub fn stop_driver(&self, id: &str) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        let mut driver = self.driver(id)?;
        require_mutable(driver.historical)?;
        if let Some(cancel) = self
            .active_drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
        {
            cancel.cancel();
        }
        driver.state = "stopped".into();
        self.save_driver(&driver)?;
        Ok(driver)
    }

    pub fn resume_driver(&self, id: &str) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        let mut driver = self.driver(id)?;
        if driver.historical {
            return self.resume_historical_driver(driver);
        }
        let lease = self.admit_suite(&driver.suite_id)?;
        if matches!(driver.state.as_str(), "running" | "waiting" | "complete") {
            return Err(BenchmarkError::Busy);
        }
        if self.suite_driver_running(&driver.suite_id)? {
            return Err(BenchmarkError::Busy);
        }
        let suite = self
            .suite(&driver.suite_id)?
            .ok_or(BenchmarkError::NotFound)?;
        require_mutable(suite.historical)?;
        let input: BenchmarkLaunchRequest = self.storage.read(|connection| {
            let bytes: Option<Vec<u8>> = connection.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [id],
                |row| row.get(0),
            )?;
            serde_json::from_slice(&bytes.ok_or(BenchmarkError::Unavailable)?)
                .map_err(|_| BenchmarkError::Unavailable)
        })?;
        if input.instance_id.as_ref().map(InstanceId::as_str) != Some(suite.instance_id.as_str())
            || input.suite_id.as_deref() != Some(suite.suite_id.as_str())
            || input.suite_mode.as_deref() != Some(suite.mode.as_str())
        {
            return Err(BenchmarkError::Unavailable);
        }
        driver.state = "running".into();
        driver.error = None;
        self.save_driver(&driver)?;
        drop(lease);
        self.run_driver(driver.clone(), input)?;
        Ok(driver)
    }

    /// Reconcile an uncertain historical Resume response without scheduling work.
    pub fn resumed_driver(
        &self,
        source_id: &str,
    ) -> Result<Option<BenchmarkSuiteDriverStatus>, BenchmarkError> {
        let source = self.driver(source_id)?;
        if !source.historical {
            return Ok(None);
        }
        self.storage
            .read(|connection| resumed_driver_in(connection, source_id))
    }

    pub fn can_resume_driver(&self, id: &str) -> Result<bool, BenchmarkError> {
        let driver = self.driver(id)?;
        Ok(self
            .resume_actions(&[driver])?
            .get(id)
            .is_some_and(|action| action.0))
    }

    /// One bounded projection shares driver activity, source links and suite
    /// proofs. These hints never replace exact command admission.
    pub fn resume_actions(
        &self,
        drivers: &[BenchmarkSuiteDriverStatus],
    ) -> Result<BTreeMap<String, (bool, Option<String>)>, BenchmarkError> {
        if drivers.len() > 64 {
            return Err(BenchmarkError::Invalid);
        }
        let candidates = self.storage.read(|connection| {
            let current = stored_drivers_in(connection)?;
            let active = current.iter().filter(|driver| matches!(driver.state.as_str(), "running" | "waiting"))
                .map(|driver| driver.suite_id.as_str()).collect::<BTreeSet<_>>();
            let by_id = current.iter().map(|driver| (driver.id.as_str(), driver)).collect::<BTreeMap<_, _>>();
            let mut links = connection.prepare("SELECT source_driver_id,driver_id FROM benchmark_drivers WHERE source_driver_id IS NOT NULL")?;
            let links = links.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let mut suites: BTreeMap<String, Option<(String, String, InstanceId)>> = BTreeMap::new();
            let mut actions = BTreeMap::new();
            for requested in drivers {
                let driver = by_id.get(requested.id.as_str()).ok_or(BenchmarkError::NotFound)?;
                let (available, successor, instance) = if !driver.historical {
                    (matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted") && !active.contains(driver.suite_id.as_str()), None, None)
                } else if has_queued_handoff(driver) {
                    (false, None, None)
                } else if let Some(successor) = links.get(&driver.id) {
                    (false, Some(successor.clone()), None)
                } else {
                    if !suites.contains_key(&driver.suite_id) {
                        let candidate = match continuation_in(connection, &driver.id) {
                            Ok((_, source, successor)) => {
                                let suite = successor.as_ref().unwrap_or(&source);
                                Some((suite.mode.clone(), suite.suite_id.clone(), suite.instance_id.parse().map_err(|_| BenchmarkError::Unavailable)?))
                            }
                            Err(BenchmarkError::Storage(error)) => return Err(BenchmarkError::Storage(error)),
                            Err(_) => None,
                        };
                        suites.insert(driver.suite_id.clone(), candidate);
                    }
                    match &suites[&driver.suite_id] {
                        Some((mode, suite_id, instance)) => (driver.mode == *mode && !active.contains(suite_id.as_str()), None, Some(instance.clone())),
                        None => (false, None, None),
                    }
                };
                actions.insert(driver.id.clone(), (available, successor, instance));
            }
            Ok::<_, BenchmarkError>(actions)
        })?;
        let mut live = BTreeMap::new();
        Ok(candidates
            .into_iter()
            .map(|(id, (mut available, successor, instance))| {
                if let Some(instance) = instance {
                    available &= *live
                        .entry(instance.clone())
                        .or_insert_with(|| self.registry.get_live(&instance).is_ok());
                }
                (id, (available, successor))
            })
            .collect())
    }

    fn resume_historical_driver(
        &self,
        source_driver: BenchmarkSuiteDriverStatus,
    ) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        // The predecessor cleared its active session when queuing this handoff.
        // Preserving that snapshot cannot authorize a replacement continuation.
        if has_queued_handoff(&source_driver) {
            return Err(BenchmarkError::Invalid);
        }
        let _source_lease = self.admit_suite(&source_driver.suite_id)?;
        if let Some(existing) = self
            .storage
            .read(|connection| resumed_driver_in(connection, &source_driver.id))?
        {
            return Ok(existing);
        }
        let (source_driver, source, existing) = self
            .storage
            .read(|connection| continuation_in(connection, &source_driver.id))?;
        let suite = existing.clone().unwrap_or_else(|| {
            let mut suite = source.clone();
            suite.suite_id = format!("suite-{}", uuid::Uuid::new_v4().simple());
            suite.historical = false;
            suite.created_at = now();
            suite.updated_at = suite.created_at.clone();
            for run in &mut suite.runs {
                if run.state == "pending" {
                    run.launch_intent = Some(uuid::Uuid::new_v4().to_string());
                }
            }
            suite
        });
        let lease = self.admit_suite(&suite.suite_id)?;
        if self.suite_driver_running(&suite.suite_id)? {
            return Err(BenchmarkError::Busy);
        }
        let request = BenchmarkLaunchRequest {
            instance_id: Some(
                suite
                    .instance_id
                    .parse()
                    .map_err(|_| BenchmarkError::Unavailable)?,
            ),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
            profile: None,
            run_type: None,
            benchmark_mode: None,
            suite_mode: Some(suite.mode.clone()),
            suite_id: Some(suite.suite_id.clone()),
            run_index: None,
            interval_ms: Some(source_driver.interval_ms),
        };
        self.launch_request(&request)?;
        let timestamp = now();
        let driver = BenchmarkSuiteDriverStatus {
            id: format!("benchmark-suite-driver-{}", uuid::Uuid::new_v4().simple()),
            suite_id: suite.suite_id.clone(),
            mode: suite.mode.clone(),
            state: "running".into(),
            interval_ms: source_driver.interval_ms,
            run_count: suite.runs.len(),
            launched_run_count: suite
                .runs
                .iter()
                .filter(|run| run.session_id.is_some())
                .count(),
            pending_run_index: suite.runs.iter().position(|run| run.state == "pending"),
            active_session_id: None,
            last_run_index: None,
            last_session_id: None,
            error: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            historical: false,
        };
        let accepted = self.storage.transaction(|tx| {
            if let Some(accepted) = resumed_driver_in(tx, &source_driver.id)? { return Ok((accepted, false)); }
            let (current_driver, current_source, current_suite) = continuation_in(tx, &source_driver.id)?;
            if current_driver != source_driver || current_source != source || current_suite != existing {
                return Err(BenchmarkError::Busy);
            }
            if existing.is_none() {
                let payload = serde_json::to_vec(&suite).map_err(|_| BenchmarkError::Unavailable)?;
                if tx.execute("INSERT INTO benchmark_suites(suite_id,payload,source_suite_id) VALUES(?1,?2,?3)", params![suite.suite_id, payload, source.suite_id])? != 1 {
                    return Err(BenchmarkError::Unavailable);
                }
            }
            let payload = serde_json::to_vec(&driver).map_err(|_| BenchmarkError::Unavailable)?;
            let captured = serde_json::to_vec(&request).map_err(|_| BenchmarkError::Unavailable)?;
            if tx.execute("INSERT INTO benchmark_drivers(driver_id,payload,request,source_driver_id) VALUES(?1,?2,?3,?4)", params![driver.id, payload, captured, source_driver.id])? != 1 {
                return Err(BenchmarkError::Unavailable);
            }
            if stored_driver(tx, &driver.id)?.as_ref() != Some(&driver) { return Err(BenchmarkError::Unavailable); }
            Ok((driver.clone(), true))
        })?;
        drop(lease);
        if accepted.1 {
            self.run_driver(accepted.0.clone(), request)?;
        }
        Ok(accepted.0)
    }

    fn run_driver(
        &self,
        driver: BenchmarkSuiteDriverStatus,
        mut input: BenchmarkLaunchRequest,
    ) -> Result<(), BenchmarkError> {
        require_mutable(driver.historical)?;
        input.suite_id = Some(driver.suite_id.clone());
        input.suite_mode = Some(driver.mode.clone());
        input.run_index = None;
        let cancel = CancellationToken::new();
        let mut active = self
            .active_drivers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if active.contains_key(&driver.id) {
            return Err(BenchmarkError::Busy);
        }
        active.insert(driver.id.clone(), cancel.clone());
        drop(active);
        let service = self.clone();
        let id = driver.id.clone();
        let handle = self.tasks.try_spawn((), move |shutdown| async move {
            loop {
                if cancel.is_cancelled() || shutdown.is_cancelled() { break; }
                let tick = service.tick(input.clone()).await;
                let Ok(mut current) = service.driver(&id) else { break; };
                if current.state == "stopped" { break; }
                let Ok(Some(suite)) = service.suite(&current.suite_id) else {
                    current.state = "failed".into();
                    current.error = Some("Benchmark suite evidence is unavailable".into());
                    let _ = service.save_driver(&current);
                    break;
                };
                current.pending_run_index = suite.runs.iter().position(|run| run.state == "pending");
                current.launched_run_count = suite.runs.iter().filter(|run| run.session_id.is_some()).count();
                match tick {
                    Ok(value) => {
                        current.state = match value["state"].as_str() {
                            Some("complete") => "complete",
                            Some("interrupted") => "interrupted",
                            _ => "waiting",
                        }.into();
                        if current.state == "interrupted" {
                            current.error = Some("An accepted benchmark session was interrupted; its process outcome is unknown".into());
                        }
                        current.active_session_id = value.get("active_session_id").and_then(|v| v.as_str()).or_else(|| value["session"]["session_id"].as_str()).map(str::to_owned);
                        if let Some(index) = value["run_index"].as_u64() { current.last_run_index = Some(index as usize); current.last_session_id = current.active_session_id.clone(); }
                    },
                    Err(error) => {
                        let settled = current.active_session_id.as_ref()
                            .and_then(|id| suite.runs.iter().find(|run| run.session_id.as_ref() == Some(id)))
                            .and_then(|run| run.launch_intent.as_deref())
                            .and_then(|key| service.launches.intent(key).ok().flatten())
                            .is_some_and(|status| matches!(status,
                                LaunchIntentStatus::Accepted { session }
                                    if session.phase == SessionPhase::Exited
                                        && Some(&session.session_id) == current.active_session_id.as_ref()
                                        && session.instance_id.as_str() == suite.instance_id));
                        if settled { current.active_session_id = None; }
                        current.state = "failed".into();
                        current.error = Some(error.to_string());
                    },
                }
                if service.save_driver(&current).is_err() || matches!(current.state.as_str(), "complete" | "failed" | "interrupted") { break; }
                tokio::select! { _ = cancel.cancelled() => break, _ = shutdown.cancelled() => break, _ = tokio::time::sleep(std::time::Duration::from_millis(current.interval_ms)) => {} }
            }
            if let Ok(mut current) = service.driver(&id) { if matches!(current.state.as_str(), "running" | "waiting") { current.state = if cancel.is_cancelled() { "stopped" } else { "interrupted" }.into(); let _ = service.save_driver(&current); } }
            service.active_drivers.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        });
        if handle.is_err() {
            self.active_drivers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&driver.id);
            let mut failed = driver;
            failed.state = "failed".into();
            failed.error = Some("Benchmark task owner refused work".into());
            self.save_driver(&failed)?;
            return Err(BenchmarkError::Busy);
        }
        Ok(())
    }

    fn save_suite(&self, suite: &BenchmarkSuiteManifest) -> Result<(), BenchmarkError> {
        require_mutable(suite.historical)?;
        let mut suite = suite.clone();
        suite.updated_at = now();
        let bytes = serde_json::to_vec(&suite).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            let (_, source) = read_suite(tx, &suite.suite_id)?.ok_or(BenchmarkError::NotFound)?;
            validate_suite_in(tx, &suite, source.as_deref())?;
            if tx.execute(
                "UPDATE benchmark_suites SET payload=?1 WHERE suite_id=?2",
                params![bytes, suite.suite_id],
            )? != 1
            {
                return Err(BenchmarkError::NotFound);
            }
            Ok(())
        })
    }
    fn save_driver(&self, driver: &BenchmarkSuiteDriverStatus) -> Result<(), BenchmarkError> {
        require_mutable(driver.historical)?;
        validate_driver(driver)?;
        let mut driver = driver.clone();
        driver.updated_at = now();
        let bytes = serde_json::to_vec(&driver).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            let (request, source): (Option<Vec<u8>>, Option<String>) = tx
                .query_row(
                    "SELECT request,source_driver_id FROM benchmark_drivers WHERE driver_id=?1",
                    [&driver.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(BenchmarkError::NotFound)?;
            validate_driver_link(
                tx,
                &driver,
                request.as_deref(),
                source.as_deref(),
                &mut BTreeMap::new(),
            )?;
            if tx.execute(
                "UPDATE benchmark_drivers SET payload=?1 WHERE driver_id=?2",
                params![bytes, driver.id],
            )? != 1
            {
                return Err(BenchmarkError::NotFound);
            }
            Ok(())
        })
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn benchmark_scenario(profile: &str, run_type: &str, mode: &str, id: &str) -> LaunchProofScenario {
    // Descriptors never authorize changing the configured Performance mode.
    LaunchProofScenario {
        benchmark_profile: Some(profile.into()),
        benchmark_run_type: Some(run_type.into()),
        benchmark_mode: Some(mode.into()),
        benchmark_id: Some(id.into()),
        ..LaunchProofScenario::default()
    }
}
fn validate_descriptor(profile: &str, run_type: &str, mode: &str) -> Result<(), BenchmarkError> {
    let matrix = benchmark_matrix();
    if valid_token(profile) && valid_token(run_type) && matrix.modes.iter().any(|p| p.id == mode) {
        Ok(())
    } else {
        Err(BenchmarkError::Invalid)
    }
}
fn validate_suite(suite: &BenchmarkSuiteManifest) -> Result<(), BenchmarkError> {
    if suite.historical {
        return validate_historical_suite(suite);
    }
    validate_live_suite(suite, None)
}

fn validate_live_suite(
    suite: &BenchmarkSuiteManifest,
    source: Option<&BenchmarkSuiteManifest>,
) -> Result<(), BenchmarkError> {
    if suite.schema != "axial.launch.benchmark.suite"
        || suite.schema_version != 2
        || !valid_token(&suite.suite_id)
        || suite.suite_id.starts_with("legacy-")
        || suite.instance_id.parse::<InstanceId>().is_err()
        || !current_plan_matches(suite)
        || chrono::DateTime::parse_from_rfc3339(&suite.created_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&suite.updated_at).is_err()
    {
        return Err(BenchmarkError::Unavailable);
    }
    for (index, run) in suite.runs.iter().enumerate() {
        if let Some(inherited) = source
            .and_then(|source| source.runs.get(index))
            .filter(|run| run.state != "pending")
        {
            if run != inherited {
                return Err(BenchmarkError::Unavailable);
            }
            continue;
        }
        if run.launch_intent.as_deref().is_none_or(|id| {
            !uuid::Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id)
        }) || !matches!(
            run.state.as_str(),
            "pending"
                | "launching"
                | "running"
                | "exited"
                | "failed"
                | "stopped"
                | "unknown"
                | "interrupted"
        ) || run.session_id.as_deref().is_some_and(|id| !valid_token(id))
        {
            return Err(BenchmarkError::Unavailable);
        }
    }
    Ok(())
}

fn imported_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

fn historical_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.+".contains(&c))
        && crate::launch::logs::Redactor::new(Vec::new()).redact_line(value) == value
}

fn historical_time(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, BenchmarkError> {
    if value.len() > 40 {
        return Err(BenchmarkError::Unavailable);
    }
    chrono::DateTime::parse_from_rfc3339(value).map_err(|_| BenchmarkError::Unavailable)
}

fn validate_historical_suite(suite: &BenchmarkSuiteManifest) -> Result<(), BenchmarkError> {
    let updated_at = historical_time(&suite.updated_at)?;
    if suite.schema != "axial.launch.benchmark.suite"
        || suite.schema_version != 2
        || !imported_id(&suite.suite_id, "legacy-suite-")
        || suite.instance_id.parse::<InstanceId>().is_err()
        || !matches!(
            suite.mode.as_str(),
            "development" | "qualification" | "release_validation"
        )
        || historical_time(&suite.created_at)? > updated_at
        || suite.runs.is_empty()
        || suite.runs.len() > 64
    {
        return Err(BenchmarkError::Unavailable);
    }
    let mut indices = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    for run in &suite.runs {
        if run.run_index >= 64
            || !indices.insert(run.run_index)
            || !historical_token(&run.profile)
            || !historical_token(&run.run_type)
            || (!run.target_id.is_empty() && !historical_token(&run.target_id))
            || !run
                .benchmark_id
                .strip_prefix("benchmark-")
                .is_some_and(|suffix| {
                    suffix.len() == 16
                        && suffix
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
            || run.launch_intent.is_some()
        {
            return Err(BenchmarkError::Unavailable);
        }
        match (
            run.state.as_str(),
            run.session_id.as_deref(),
            run.launched_at.as_deref(),
        ) {
            ("pending", None, None) => {}
            ("failed" | "stopped" | "exited" | "completed", Some(session), Some(launched)) => {
                if !imported_id(session, "legacy-")
                    || !sessions.insert(session)
                    || historical_time(launched)? > updated_at
                {
                    return Err(BenchmarkError::Unavailable);
                }
            }
            _ => return Err(BenchmarkError::Unavailable),
        }
    }
    Ok(())
}

fn validate_driver(driver: &BenchmarkSuiteDriverStatus) -> Result<(), BenchmarkError> {
    if !driver.historical {
        return if driver.id.starts_with("legacy-") || driver.suite_id.starts_with("legacy-") {
            Err(BenchmarkError::Unavailable)
        } else {
            Ok(())
        };
    }
    if !imported_id(&driver.id, "legacy-driver-")
        || !imported_id(&driver.suite_id, "legacy-suite-")
        || !matches!(
            driver.mode.as_str(),
            "development" | "qualification" | "release_validation"
        )
        || !matches!(
            driver.state.as_str(),
            "complete" | "failed" | "stopped" | "interrupted"
        )
        || !(5_000..=3_600_000).contains(&driver.interval_ms)
        || driver.run_count == 0
        || driver.run_count > 64
        || driver.launched_run_count > driver.run_count
        || driver
            .pending_run_index
            .is_some_and(|index| index >= driver.run_count)
        || driver
            .last_run_index
            .is_some_and(|index| index >= driver.run_count)
        || driver.active_session_id.is_some()
        || (driver.state == "complete" && driver.pending_run_index.is_some())
        || driver
            .last_session_id
            .as_deref()
            .is_some_and(|id| !imported_id(id, "legacy-") || driver.last_run_index.is_none())
        || historical_time(&driver.created_at)? > historical_time(&driver.updated_at)?
        || driver.error.as_deref().is_some_and(|error| {
            error.chars().count() > 160
                || error.contains(['/', '\\'])
                || error.trim() != error
                || error.chars().any(char::is_control)
                || crate::launch::logs::Redactor::new(Vec::new()).redact_line(error) != error
                || (matches!(
                    error,
                    "driver automatic resume queued after restart"
                        | "driver automatic resume started after restart"
                        | "driver ignored after restart resume limit"
                ) && driver.state != "interrupted")
        })
    {
        return Err(BenchmarkError::Unavailable);
    }
    Ok(())
}

pub fn driver_payload(driver: BenchmarkSuiteDriverStatus) -> serde_json::Value {
    let running = !driver.historical && matches!(driver.state.as_str(), "running" | "waiting");
    let resumable =
        !driver.historical && matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted");
    let state_label = if driver.historical {
        format!("Historical {} (read-only)", driver.state)
    } else {
        driver.state.clone()
    };
    serde_json::json!({"status":"ok","suite":{"suite_id":driver.suite_id,"mode":driver.mode,"run_count":driver.run_count,
        "launched_run_count":driver.launched_run_count,"pending_run_index":driver.pending_run_index},
        "view_model":{"state_label":state_label,"state_tone":if driver.state=="complete" {"ok"} else if matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted") {"warn"} else {"neutral"},
            "can_stop":running,"can_resume":resumable,"can_check_family_c_qualification":driver.mode=="release_validation"},"driver":driver})
}

pub fn qualification_preview() -> serde_json::Value {
    let now = now();
    let manifest = BenchmarkSuiteManifest {
        schema: "axial.launch.benchmark.suite".into(),
        schema_version: 2,
        suite_id: "preview".into(),
        instance_id: String::new(),
        mode: "release_validation".into(),
        created_at: now.clone(),
        updated_at: now,
        runs: Vec::new(),
        historical: false,
    };
    super::qualification::qualification_payload(&manifest, &[], None, false)
}
