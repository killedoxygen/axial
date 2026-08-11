use crate::execution::anchored_record::AnchoredRecordTarget;
use crate::state::contracts::OperationId;
use axial_fs::RootStateSuccessor;
use std::collections::BTreeSet;
use std::io;

const SNAPSHOT_SUCCESSOR_SCHEMA: u16 = 1;
const PERFORMANCE_OPERATION_SUCCESSOR_OWNER: &[u8] = b"performance-operation";
const PERFORMANCE_OPERATION_SUCCESSOR_PARENT: &[&str] = &["performance", "operations"];

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

fn admits_performance_operation_batch<'a>(
    owner_schema: u16,
    owner_id: &[u8],
    count: usize,
    mut destination: impl FnMut(usize) -> Option<(Vec<&'a str>, &'a str)>,
) -> bool {
    if owner_schema != SNAPSHOT_SUCCESSOR_SCHEMA
        || owner_id != PERFORMANCE_OPERATION_SUCCESSOR_OWNER
        || !(1..=32).contains(&count)
    {
        return false;
    }
    let mut operations = BTreeSet::new();
    (0..count).all(|index| {
        let Some((parent, leaf)) = destination(index) else {
            return false;
        };
        parent == PERFORMANCE_OPERATION_SUCCESSOR_PARENT
            && performance_operation_from_leaf(leaf)
                .is_some_and(|operation| operations.insert(operation))
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
        PERFORMANCE_OPERATION_SUCCESSOR_OWNER, admits_performance_operation_batch, matching_spec,
        performance_operation_from_leaf,
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
}
