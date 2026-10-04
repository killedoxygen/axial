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
        rusqlite::{self, Connection, OptionalExtension, params},
    },
    tasks::{CancellationToken, TaskOwner},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[path = "benchmarks_tests.rs"]
mod recovery_tests;

const MATRIX_SCHEMA: &str = "axial.launch.benchmark.matrix";
const MATRIX_SCHEMA_VERSION: u32 = 1;
const MAX_MATRIX_JSON_BYTES: usize = 12 * 1024;
const MAX_RESTART_DRIVERS: usize = 8;
const MAX_STORED_DRIVERS: usize = 4096;
const RESTART_INTERRUPTED_ERROR: &str = "Driver interrupted by application restart";
const RESTART_LIMIT_ERROR: &str = "driver ignored after restart resume limit";

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

/// Stable redacted descriptor identity.
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
}

fn verify_driver_capacity(db: &Connection) -> Result<(), BenchmarkError> {
    let count: usize = db.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM benchmark_drivers LIMIT ?1)",
        [MAX_STORED_DRIVERS + 1],
        |row| row.get(0),
    )?;
    if count > MAX_STORED_DRIVERS {
        return Err(BenchmarkError::Unavailable);
    }
    Ok(())
}

fn stored_suite(
    connection: &Connection,
    id: &str,
) -> Result<Option<BenchmarkSuiteManifest>, BenchmarkError> {
    let record: Option<Vec<u8>> = connection
        .query_row(
            "SELECT payload FROM benchmark_suites WHERE suite_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    record
        .map(|bytes| {
            let suite: BenchmarkSuiteManifest =
                serde_json::from_slice(&bytes).map_err(|_| BenchmarkError::Unavailable)?;
            if suite.suite_id != id {
                return Err(BenchmarkError::Unavailable);
            }
            validate_suite(&suite)?;
            Ok(suite)
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

fn stored_driver(
    connection: &Connection,
    id: &str,
) -> Result<Option<BenchmarkSuiteDriverStatus>, BenchmarkError> {
    let record: Option<Vec<u8>> = connection
        .query_row(
            "SELECT payload FROM benchmark_drivers WHERE driver_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    record.map(|bytes| decode_driver(id, &bytes)).transpose()
}

fn stored_drivers_in(
    connection: &Connection,
) -> Result<Vec<BenchmarkSuiteDriverStatus>, BenchmarkError> {
    let mut query = connection
        .prepare("SELECT driver_id,payload FROM benchmark_drivers ORDER BY rowid DESC LIMIT ?1")?;
    let mut rows = query.query([MAX_STORED_DRIVERS + 1])?;
    let mut drivers = Vec::new();
    while let Some(row) = rows.next()? {
        if drivers.len() == MAX_STORED_DRIVERS {
            return Err(BenchmarkError::Unavailable);
        }
        let id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        drivers.push(decode_driver(&id, &bytes)?);
    }
    Ok(drivers)
}

fn decode_driver(id: &str, bytes: &[u8]) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
    let driver: BenchmarkSuiteDriverStatus =
        serde_json::from_slice(bytes).map_err(|_| BenchmarkError::Unavailable)?;
    if driver.id != id {
        return Err(BenchmarkError::Unavailable);
    }
    Ok(driver)
}

fn stored_resume_request(
    connection: &Connection,
    driver: &BenchmarkSuiteDriverStatus,
    suite: &BenchmarkSuiteManifest,
) -> Result<Option<BenchmarkLaunchRequest>, BenchmarkError> {
    let bytes: Option<Vec<u8>> = connection.query_row(
        "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
        [&driver.id],
        |row| row.get(0),
    )?;
    Ok(bytes
        .and_then(|bytes| serde_json::from_slice::<BenchmarkLaunchRequest>(&bytes).ok())
        .filter(|input| {
            input.instance_id.as_ref().map(InstanceId::as_str) == Some(suite.instance_id.as_str())
                && input.suite_id.as_deref() == Some(suite.suite_id.as_str())
                && input.suite_mode.as_deref() == Some(suite.mode.as_str())
        }))
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
            if matches!(driver.state.as_str(), "running" | "waiting") {
                driver.state = "interrupted".into();
                driver.error = Some(RESTART_INTERRUPTED_ERROR.into());
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
        let candidates = self
            .stored_drivers()?
            .into_iter()
            .filter(|driver| {
                driver.state == "interrupted"
                    && driver.error.as_deref() == Some(RESTART_INTERRUPTED_ERROR)
            })
            .collect::<Vec<_>>();
        for driver in candidates.iter().skip(MAX_RESTART_DRIVERS) {
            self.checkpoint_automatic_resume(driver, RESTART_LIMIT_ERROR)?;
        }
        let mut resumed = 0;
        for driver in candidates.iter().take(MAX_RESTART_DRIVERS) {
            let failure = self.storage.read(|connection| {
                let Some(suite) = stored_suite(connection, &driver.suite_id)? else {
                    return Ok(Some(
                        "driver automatic resume failed: benchmark suite not found",
                    ));
                };
                Ok::<_, BenchmarkError>(
                    stored_resume_request(connection, driver, &suite)?
                        .is_none()
                        .then_some(
                            "driver automatic resume failed: benchmark launch request is invalid",
                        ),
                )
            })?;
            if let Some(failure) = failure {
                self.checkpoint_automatic_resume(driver, failure)?;
                continue;
            }
            self.resume_driver(&driver.id)?;
            resumed += 1;
        }
        Ok(resumed)
    }

    fn checkpoint_automatic_resume(
        &self,
        expected: &BenchmarkSuiteDriverStatus,
        error: &str,
    ) -> Result<(), BenchmarkError> {
        let mut updated = expected.clone();
        updated.error = Some(error.into());
        updated.updated_at = now();
        let payload = serde_json::to_vec(&updated).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            if stored_driver(tx, &expected.id)?.as_ref() != Some(expected) {
                return Err(BenchmarkError::Busy);
            }
            let request: Option<Vec<u8>> = tx.query_row(
                "SELECT request FROM benchmark_drivers WHERE driver_id=?1",
                [&expected.id],
                |row| row.get(0),
            )?;
            if tx.execute(
                "UPDATE benchmark_drivers SET payload=?1 WHERE driver_id=?2",
                params![payload, expected.id],
            )? != 1
            {
                return Err(BenchmarkError::Unavailable);
            }
            let saved: (Vec<u8>, Option<Vec<u8>>) = tx.query_row(
                "SELECT payload,request FROM benchmark_drivers WHERE driver_id=?1",
                [&expected.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if saved != (payload, request)
                || stored_driver(tx, &expected.id)?.as_ref() != Some(&updated)
            {
                return Err(BenchmarkError::Unavailable);
            }
            Ok(())
        })
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
        if let Some(existing) = self.suite(&id)? {
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
        let mut preparing_index = None;
        for index in 0..suite.runs.len() {
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
                // A benchmark may have crashed before mapping its session.
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
            if tx.execute(
                "INSERT INTO benchmark_drivers(driver_id,payload,request) VALUES(?1,?2,?3)",
                params![driver.id, payload, request],
            )? != 1
            {
                return Err(BenchmarkError::Unavailable);
            }
            verify_driver_capacity(tx)?;
            Ok::<_, BenchmarkError>(())
        })?;
        drop(lease);
        self.run_driver(driver.clone(), captured)?;
        Ok(driver)
    }

    pub fn stop_driver(&self, id: &str) -> Result<BenchmarkSuiteDriverStatus, BenchmarkError> {
        let mut driver = self.driver(id)?;
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
        let input = self.storage.read(|connection| {
            stored_resume_request(connection, &driver, &suite)?.ok_or(BenchmarkError::Unavailable)
        })?;
        driver.state = "running".into();
        driver.error = None;
        self.save_driver(&driver)?;
        drop(lease);
        self.run_driver(driver.clone(), input)?;
        Ok(driver)
    }

    pub fn can_resume_driver(&self, id: &str) -> Result<bool, BenchmarkError> {
        let driver = self.driver(id)?;
        Ok(self
            .resume_actions(&[driver])?
            .get(id)
            .copied()
            .unwrap_or(false))
    }

    /// One bounded projection shares driver activity. These hints never
    /// replace exact command admission.
    pub fn resume_actions(
        &self,
        drivers: &[BenchmarkSuiteDriverStatus],
    ) -> Result<BTreeMap<String, bool>, BenchmarkError> {
        if drivers.len() > 64 {
            return Err(BenchmarkError::Invalid);
        }
        self.storage.read(|connection| {
            let current = stored_drivers_in(connection)?;
            let active = current
                .iter()
                .filter(|driver| matches!(driver.state.as_str(), "running" | "waiting"))
                .map(|driver| driver.suite_id.as_str())
                .collect::<BTreeSet<_>>();
            let by_id = current
                .iter()
                .map(|driver| (driver.id.as_str(), driver))
                .collect::<BTreeMap<_, _>>();
            drivers
                .iter()
                .map(|requested| {
                    let driver = by_id
                        .get(requested.id.as_str())
                        .ok_or(BenchmarkError::NotFound)?;
                    let available =
                        matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted")
                            && !active.contains(driver.suite_id.as_str());
                    Ok((driver.id.clone(), available))
                })
                .collect()
        })
    }

    fn run_driver(
        &self,
        driver: BenchmarkSuiteDriverStatus,
        mut input: BenchmarkLaunchRequest,
    ) -> Result<(), BenchmarkError> {
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
        let mut suite = suite.clone();
        suite.updated_at = now();
        let bytes = serde_json::to_vec(&suite).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
            validate_suite(&suite)?;
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
        let mut driver = driver.clone();
        driver.updated_at = now();
        let bytes = serde_json::to_vec(&driver).map_err(|_| BenchmarkError::Unavailable)?;
        self.storage.transaction(|tx| {
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
    if suite.schema != "axial.launch.benchmark.suite"
        || suite.schema_version != 2
        || !valid_token(&suite.suite_id)
        || suite.instance_id.parse::<InstanceId>().is_err()
        || !current_plan_matches(suite)
        || chrono::DateTime::parse_from_rfc3339(&suite.created_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&suite.updated_at).is_err()
    {
        return Err(BenchmarkError::Unavailable);
    }
    for run in &suite.runs {
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

pub fn driver_payload(driver: BenchmarkSuiteDriverStatus) -> serde_json::Value {
    let running = matches!(driver.state.as_str(), "running" | "waiting");
    let resumable = matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted");
    serde_json::json!({"status":"ok","suite":{"suite_id":driver.suite_id,"mode":driver.mode,"run_count":driver.run_count,
        "launched_run_count":driver.launched_run_count,"pending_run_index":driver.pending_run_index},
        "view_model":{"state_label":driver.state,"state_tone":if driver.state=="complete" {"ok"} else if matches!(driver.state.as_str(), "stopped" | "failed" | "interrupted") {"warn"} else {"neutral"},
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
    };
    super::qualification::qualification_payload(&manifest, &[], None, false)
}
