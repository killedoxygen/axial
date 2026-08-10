use crate::execution::anchored_record::AnchoredRecordTarget;
use axial_fs::RootStateSuccessor;
use std::io;

const SNAPSHOT_SUCCESSOR_SCHEMA: u16 = 1;

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
pub(super) const OPERATION_JOURNAL_SUCCESSOR: StateSnapshotSuccessorSpec =
    StateSnapshotSuccessorSpec {
        owner_id: b"operation-journals",
        parent: &["state"],
        leaf: "operation-journals.json",
    };

const STARTUP_SNAPSHOT_SUCCESSORS: [StateSnapshotSuccessorSpec; 3] = [
    CONFIG_SNAPSHOT_SUCCESSOR,
    INSTANCE_REGISTRY_SUCCESSOR,
    OPERATION_JOURNAL_SUCCESSOR,
];

pub(crate) fn admit_startup_state_successor(successor: &RootStateSuccessor) -> io::Result<()> {
    let destination = successor.recovery_destination(0);
    let admitted = destination.as_ref().and_then(|(parent, leaf)| {
        matching_spec(
            successor.owner_schema(),
            successor.owner_id(),
            successor.recovery_count(),
            parent,
            leaf,
        )
    });
    if admitted.is_none()
        || !successor_payload_matches_proof(
            successor.old_payload(),
            successor.recovery_old_proof(0),
        )
        || successor.new_payload().is_none()
        || !successor_payload_matches_proof(
            successor.new_payload(),
            successor.recovery_new_proof(0),
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "State successor does not describe an admitted startup snapshot",
        ));
    }
    Ok(())
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

fn successor_payload_matches_proof(payload: Option<&[u8]>, proof: Option<(u64, [u8; 32])>) -> bool {
    match (payload, proof) {
        (None, None) => true,
        (Some(payload), Some((size, sha256))) => {
            let mut expected = [0; 40];
            expected[..8].copy_from_slice(&size.to_le_bytes());
            expected[8..].copy_from_slice(&sha256);
            payload == expected
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{matching_spec, successor_payload_matches_proof};

    #[test]
    fn startup_successor_registry_is_exact_and_closed() {
        for (owner, parent, leaf) in [
            (b"config".as_slice(), &[][..], "config.json"),
            (b"instance-registry".as_slice(), &[][..], "instances.json"),
            (
                b"operation-journals".as_slice(),
                &["state"][..],
                "operation-journals.json",
            ),
        ] {
            assert!(matching_spec(1, owner, 1, parent, leaf).is_some());
            assert!(matching_spec(2, owner, 1, parent, leaf).is_none());
            assert!(matching_spec(1, owner, 2, parent, leaf).is_none());
            assert!(matching_spec(1, owner, 1, parent, "other.json").is_none());
        }
        assert!(matching_spec(1, b"config", 1, &["state"], "config.json").is_none());
        assert!(matching_spec(1, b"unknown", 1, &[], "config.json").is_none());
    }

    #[test]
    fn successor_payload_is_the_exact_canonical_file_proof() {
        let proof = (73_u64, [0x41; 32]);
        let mut encoded = [0; 40];
        encoded[..8].copy_from_slice(&proof.0.to_le_bytes());
        encoded[8..].copy_from_slice(&proof.1);
        assert!(successor_payload_matches_proof(Some(&encoded), Some(proof)));
        assert!(successor_payload_matches_proof(None, None));

        let mut changed = encoded;
        changed[39] ^= 1;
        assert!(!successor_payload_matches_proof(
            Some(&changed),
            Some(proof)
        ));
        assert!(!successor_payload_matches_proof(Some(&encoded), None));
        assert!(!successor_payload_matches_proof(None, Some(proof)));
    }
}
