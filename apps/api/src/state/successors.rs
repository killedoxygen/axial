use crate::execution::anchored_record::AnchoredRecordTarget;
use crate::state::benchmark_suites::is_canonical_suite_id;
use crate::state::contracts::OperationId;
use crate::state::launch_reports::canonical_session_id;
use axial_fs::RootStateSuccessor;
use std::collections::BTreeSet;
use std::io;

const SNAPSHOT_SUCCESSOR_SCHEMA: u16 = 1;
const PERFORMANCE_OPERATION_SUCCESSOR_OWNER: &[u8] = b"performance-operation";
const PERFORMANCE_OPERATION_SUCCESSOR_PARENT: &[&str] = &["performance", "operations"];
const LAUNCH_REPORT_SUCCESSOR_OWNER: &[u8] = b"launch-report";
const LAUNCH_REPORT_SUCCESSOR_PARENT: &[&str] = &["benchmarks", "launch"];
const BENCHMARK_SUITE_SUCCESSOR_OWNER: &[u8] = b"benchmark-suite";
const BENCHMARK_SUITE_SUCCESSOR_PARENT: &[&str] = &["benchmarks", "suites"];

#[derive(Clone, Copy)]
pub(super) struct StateSnapshotSuccessorSpec {
    owner_id: &'static [u8],
    parent: &'static [&'static str],
    leaf: &'static str,
}

impl StateSnapshotSuccessorSpec {
    pub(super) fn bind(self, target: AnchoredRecordTarget) -> io::Result<AnchoredRecordTarget> {
        target.with_state_successor(SNAPSHOT_SUCCESSOR_SCHEMA, self.owner_id)
    }
}

pub(super) fn bind_performance_operation_successor(
    target: AnchoredRecordTarget,
) -> io::Result<AnchoredRecordTarget> {
    target.with_state_successor(
        SNAPSHOT_SUCCESSOR_SCHEMA,
        PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
    )
}

pub(super) fn bind_launch_report_successor(
    target: AnchoredRecordTarget,
) -> io::Result<AnchoredRecordTarget> {
    target.with_state_successor(SNAPSHOT_SUCCESSOR_SCHEMA, LAUNCH_REPORT_SUCCESSOR_OWNER)
}

pub(super) fn bind_benchmark_suite_successor(
    target: AnchoredRecordTarget,
) -> io::Result<AnchoredRecordTarget> {
    target.with_state_successor(SNAPSHOT_SUCCESSOR_SCHEMA, BENCHMARK_SUITE_SUCCESSOR_OWNER)
}

pub(super) const ACCOUNT_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"launcher-accounts",
        parent: &[],
        leaf: "accounts.json",
    };
pub(super) const CONFIG_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"config",
        parent: &[],
        leaf: "config.json",
    };
pub(super) const INSTANCE_REGISTRY_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"instance-registry",
        parent: &[],
        leaf: "instances.json",
    };
pub(super) const FAILURE_MEMORY_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"guardian-failure-memory",
        parent: &["guardian"],
        leaf: "failure-memory.json",
    };
pub(super) const OPERATION_JOURNAL_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"operation-journals",
        parent: &["state"],
        leaf: "operation-journals.json",
    };
pub(super) const PERFORMANCE_RULES_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"performance-rules",
        parent: &["performance"],
        leaf: "rules-cache.json",
    };
pub(super) const REJECTION_STREAK_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"persisted-state-rejection-streaks",
        parent: &["state"],
        leaf: "persisted-state-rejection-streaks.json",
    };
pub(super) const USER_MOD_WITNESS_SNAPSHOT_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"guardian-user-mod-witnesses",
        parent: &[],
        leaf: "guardian-user-mod-witnesses.json",
    };

const STARTUP_SNAPSHOT_SUCCESSORS: [StateSnapshotSuccessorSpec; 8] = [
    ACCOUNT_SNAPSHOT_SUCCESSOR,
    CONFIG_SNAPSHOT_SUCCESSOR,
    FAILURE_MEMORY_SNAPSHOT_SUCCESSOR,
    INSTANCE_REGISTRY_SUCCESSOR,
    OPERATION_JOURNAL_SUCCESSOR,
    PERFORMANCE_RULES_SNAPSHOT_SUCCESSOR,
    REJECTION_STREAK_SNAPSHOT_SUCCESSOR,
    USER_MOD_WITNESS_SNAPSHOT_SUCCESSOR,
];

pub(crate) fn admit_startup_state_successor(successor: &RootStateSuccessor) -> io::Result<()> {
    let admitted = admits_performance_operation_batch(
        successor.owner_schema(),
        successor.owner_id(),
        successor.recovery_count(),
        |index| successor.recovery_destination(index),
    ) || admits_launch_report_batch(
        successor.owner_schema(),
        successor.owner_id(),
        successor.recovery_count(),
        |index| successor.recovery_destination(index),
    ) || admits_benchmark_suite_batch(
        successor.owner_schema(),
        successor.owner_id(),
        successor.recovery_count(),
        |index| successor.recovery_destination(index),
    ) || successor
        .recovery_destination(0)
        .as_ref()
        .is_some_and(|(parent, leaf)| {
            matching_spec(
                successor.owner_schema(),
                successor.owner_id(),
                successor.recovery_count(),
                parent,
                leaf,
            )
            .is_some()
        });
    admitted.then_some(()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "State successor does not describe an admitted startup record",
        )
    })
}

fn admits_launch_report_batch<'a>(
    owner_schema: u16,
    owner_id: &[u8],
    count: usize,
    mut destination: impl FnMut(usize) -> Option<(Vec<&'a str>, &'a str)>,
) -> bool {
    admits_dynamic_batch(
        owner_schema,
        owner_id,
        count,
        &mut destination,
        LAUNCH_REPORT_SUCCESSOR_OWNER,
        LAUNCH_REPORT_SUCCESSOR_PARENT,
        launch_report_session_from_leaf,
    )
}

fn launch_report_session_from_leaf(leaf: &str) -> Option<&str> {
    let session = leaf.strip_suffix(".json")?;
    canonical_session_id(session).then_some(session)
}

fn admits_benchmark_suite_batch<'a>(
    owner_schema: u16,
    owner_id: &[u8],
    count: usize,
    mut destination: impl FnMut(usize) -> Option<(Vec<&'a str>, &'a str)>,
) -> bool {
    admits_dynamic_batch(
        owner_schema,
        owner_id,
        count,
        &mut destination,
        BENCHMARK_SUITE_SUCCESSOR_OWNER,
        BENCHMARK_SUITE_SUCCESSOR_PARENT,
        benchmark_suite_from_leaf,
    )
}

fn benchmark_suite_from_leaf(leaf: &str) -> Option<&str> {
    let suite_id = leaf.strip_suffix(".json")?;
    is_canonical_suite_id(suite_id).then_some(suite_id)
}

fn admits_performance_operation_batch<'a>(
    owner_schema: u16,
    owner_id: &[u8],
    count: usize,
    mut destination: impl FnMut(usize) -> Option<(Vec<&'a str>, &'a str)>,
) -> bool {
    admits_dynamic_batch(
        owner_schema,
        owner_id,
        count,
        &mut destination,
        PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
        PERFORMANCE_OPERATION_SUCCESSOR_PARENT,
        performance_operation_from_leaf,
    )
}

fn admits_dynamic_batch<'a, T: Ord>(
    owner_schema: u16,
    owner_id: &[u8],
    count: usize,
    destination: &mut impl FnMut(usize) -> Option<(Vec<&'a str>, &'a str)>,
    admitted_owner: &[u8],
    admitted_parent: &[&str],
    mut leaf_id: impl FnMut(&'a str) -> Option<T>,
) -> bool {
    if owner_schema != SNAPSHOT_SUCCESSOR_SCHEMA
        || owner_id != admitted_owner
        || !(1..=32).contains(&count)
    {
        return false;
    }
    let mut identities = BTreeSet::new();
    (0..count).all(|index| {
        let Some((parent, leaf)) = destination(index) else {
            return false;
        };
        parent == admitted_parent
            && leaf_id(leaf).is_some_and(|identity| identities.insert(identity))
    })
}

fn performance_operation_from_leaf(leaf: &str) -> Option<OperationId> {
    let encoded = leaf.strip_suffix(".json")?;
    OperationId::try_from(encoded)
        .ok()
        .filter(|operation| operation.to_string() == encoded)
}

fn matching_spec(
    owner_schema: u16,
    owner_id: &[u8],
    recovery_count: usize,
    parent: &[&str],
    leaf: &str,
) -> Option<StateSnapshotSuccessorSpec> {
    if owner_schema != SNAPSHOT_SUCCESSOR_SCHEMA || recovery_count != 1 {
        return None;
    }
    let mut matches = STARTUP_SNAPSHOT_SUCCESSORS
        .iter()
        .copied()
        .filter(|spec| spec.owner_id == owner_id && spec.parent == parent && spec.leaf == leaf);
    let admitted = matches.next()?;
    matches.next().is_none().then_some(admitted)
}

#[cfg(test)]
mod tests {
    use super::{
        BENCHMARK_SUITE_SUCCESSOR_OWNER, LAUNCH_REPORT_SUCCESSOR_OWNER,
        PERFORMANCE_OPERATION_SUCCESSOR_OWNER, admits_benchmark_suite_batch,
        admits_launch_report_batch, admits_performance_operation_batch, benchmark_suite_from_leaf,
        launch_report_session_from_leaf, matching_spec, performance_operation_from_leaf,
    };
    use crate::state::contracts::OperationId;

    #[test]
    fn startup_successor_registry_is_exact_and_closed() {
        for (owner, parent, leaf) in [
            (b"launcher-accounts".as_slice(), &[][..], "accounts.json"),
            (b"config".as_slice(), &[][..], "config.json"),
            (
                b"guardian-failure-memory".as_slice(),
                &["guardian"][..],
                "failure-memory.json",
            ),
            (b"instance-registry".as_slice(), &[][..], "instances.json"),
            (
                b"operation-journals".as_slice(),
                &["state"][..],
                "operation-journals.json",
            ),
            (
                b"performance-rules".as_slice(),
                &["performance"][..],
                "rules-cache.json",
            ),
            (
                b"persisted-state-rejection-streaks".as_slice(),
                &["state"][..],
                "persisted-state-rejection-streaks.json",
            ),
            (
                b"guardian-user-mod-witnesses".as_slice(),
                &[][..],
                "guardian-user-mod-witnesses.json",
            ),
        ] {
            assert!(matching_spec(1, owner, 1, parent, leaf).is_some());
            assert!(matching_spec(2, owner, 1, parent, leaf).is_none());
            assert!(matching_spec(1, owner, 2, parent, leaf).is_none());
            assert!(matching_spec(1, owner, 1, parent, "other.json").is_none());
        }
        assert!(matching_spec(1, b"config", 1, &["state"], "config.json").is_none());
        assert!(matching_spec(1, b"unknown", 1, &[], "config.json").is_none());
        assert!(
            matching_spec(
                1,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                1,
                &["performance", "operations"],
                "operation.json",
            )
            .is_none()
        );
    }

    #[test]
    fn performance_operation_successor_is_strict_and_dynamic() {
        let operation_id = OperationId::deterministic_test("performance-successor");
        let leaf = format!("{operation_id}.json");
        assert_eq!(performance_operation_from_leaf(&leaf), Some(operation_id));
        for candidate in [
            "operation.json".to_string(),
            "op-00000000-0000-1000-8000-000000000000.json".to_string(),
            leaf.to_uppercase(),
            format!("{leaf}.json"),
        ] {
            assert!(performance_operation_from_leaf(&candidate).is_none());
        }
    }

    #[test]
    fn performance_operation_batch_admission_is_exact_and_complete() {
        let first = OperationId::deterministic_test("performance-batch-first").to_string();
        let second = OperationId::deterministic_test("performance-batch-second").to_string();
        let leaves = [format!("{first}.json"), format!("{second}.json")];
        let admitted = |schema, owner: &[u8], count, leaves: &[String], parent: &[&str]| {
            admits_performance_operation_batch(schema, owner, count, |index| {
                leaves
                    .get(index)
                    .map(|leaf| (parent.to_vec(), leaf.as_str()))
            })
        };
        let parent = ["performance", "operations"];
        assert!(admitted(
            1,
            PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
            2,
            &leaves,
            &parent,
        ));
        assert!(!admitted(
            1,
            PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
            2,
            &[leaves[0].clone(), leaves[0].clone()],
            &parent,
        ));
        for (schema, owner, count, candidates, parent) in [
            (
                2,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                2,
                &leaves[..],
                &parent[..],
            ),
            (1, b"other".as_slice(), 2, &leaves[..], &parent[..]),
            (
                1,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                0,
                &[][..],
                &parent[..],
            ),
            (
                1,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                33,
                &leaves[..],
                &parent[..],
            ),
            (
                1,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                2,
                &leaves[..1],
                &parent[..],
            ),
            (
                1,
                PERFORMANCE_OPERATION_SUCCESSOR_OWNER,
                2,
                &leaves[..],
                &["other"][..],
            ),
        ] {
            assert!(!admitted(schema, owner, count, candidates, parent));
        }
    }

    #[test]
    fn launch_report_batch_admission_is_exact_and_complete() {
        let leaves = ["launch-session_1.json", "launch-session_2.json"];
        let admitted = |schema, owner: &[u8], count, leaves: &[&str], parent: &[&str]| {
            admits_launch_report_batch(schema, owner, count, |index| {
                leaves.get(index).map(|leaf| (parent.to_vec(), *leaf))
            })
        };
        let parent = ["benchmarks", "launch"];
        assert!(admitted(
            1,
            LAUNCH_REPORT_SUCCESSOR_OWNER,
            2,
            &leaves,
            &parent,
        ));
        assert!(!admitted(
            1,
            LAUNCH_REPORT_SUCCESSOR_OWNER,
            2,
            &[leaves[0], leaves[0]],
            &parent,
        ));
        assert!(!admitted(
            1,
            LAUNCH_REPORT_SUCCESSOR_OWNER,
            2,
            &leaves,
            &["benchmarks", "other"],
        ));
        assert!(!admitted(
            2,
            LAUNCH_REPORT_SUCCESSOR_OWNER,
            2,
            &leaves,
            &parent
        ));
        assert!(!admitted(1, b"other", 2, &leaves, &parent));
        assert!(!admitted(
            1,
            LAUNCH_REPORT_SUCCESSOR_OWNER,
            3,
            &leaves,
            &parent,
        ));
        assert_eq!(
            launch_report_session_from_leaf(leaves[0]),
            Some("launch-session_1")
        );
        for leaf in [
            "Launch-session.json",
            "launch/session.json",
            "launch-session",
        ] {
            assert!(launch_report_session_from_leaf(leaf).is_none());
        }
    }

    #[test]
    fn benchmark_suite_batch_admission_is_exact_and_complete() {
        let leaves = [
            "suite-dev-0123456789abcdef.json",
            "suite-qual-fedcba9876543210.json",
        ];
        let admitted = |schema, owner: &[u8], count, leaves: &[&str], parent: &[&str]| {
            admits_benchmark_suite_batch(schema, owner, count, |index| {
                leaves.get(index).map(|leaf| (parent.to_vec(), *leaf))
            })
        };
        let parent = ["benchmarks", "suites"];
        assert!(admitted(
            1,
            BENCHMARK_SUITE_SUCCESSOR_OWNER,
            2,
            &leaves,
            &parent,
        ));
        assert!(!admitted(
            1,
            BENCHMARK_SUITE_SUCCESSOR_OWNER,
            2,
            &[leaves[0], leaves[0]],
            &parent,
        ));
        for (schema, owner, count, candidates, parent) in [
            (
                2,
                BENCHMARK_SUITE_SUCCESSOR_OWNER,
                2,
                &leaves[..],
                &parent[..],
            ),
            (1, b"other".as_slice(), 2, &leaves[..], &parent[..]),
            (1, BENCHMARK_SUITE_SUCCESSOR_OWNER, 0, &[][..], &parent[..]),
            (
                1,
                BENCHMARK_SUITE_SUCCESSOR_OWNER,
                33,
                &leaves[..],
                &parent[..],
            ),
            (
                1,
                BENCHMARK_SUITE_SUCCESSOR_OWNER,
                2,
                &leaves[..1],
                &parent[..],
            ),
            (
                1,
                BENCHMARK_SUITE_SUCCESSOR_OWNER,
                2,
                &leaves[..],
                &["benchmarks", "other"][..],
            ),
        ] {
            assert!(!admitted(schema, owner, count, candidates, parent));
        }
        assert_eq!(
            benchmark_suite_from_leaf(leaves[0]),
            Some("suite-dev-0123456789abcdef")
        );
        for leaf in [
            "suite-dev-0123456789ABCDEf.json",
            "suite-other-0123456789abcdef.json",
            "suite-dev-0123456789abcdef",
        ] {
            assert!(benchmark_suite_from_leaf(leaf).is_none());
        }
    }
}
