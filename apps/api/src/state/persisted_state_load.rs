use crate::execution::anchored_record::{AnchoredRecordIdentity, AnchoredRecordRestartDigest};
use crate::state::contracts::{
    PersistedStateRecordStore, RestartStableRecordIdentity, TargetDescriptor,
};

pub(super) const MAX_REJECTED_RESTART_RECORDS_PER_STORE: usize = 8;
pub(super) const MAX_RESTART_RECORD_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStateRecordRejection {
    Oversized,
    InvalidSchema,
    InvalidIdentity,
    InvalidSemantics,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedStateRejectedRecordEvidence {
    store: PersistedStateRecordStore,
    rejection: PersistedStateRecordRejection,
    target: TargetDescriptor,
}

impl PersistedStateRejectedRecordEvidence {
    pub(crate) fn store(&self) -> PersistedStateRecordStore {
        self.store
    }

    pub(crate) fn rejection(&self) -> PersistedStateRecordRejection {
        self.rejection
    }

    pub(crate) fn target(&self) -> &TargetDescriptor {
        &self.target
    }
}

pub(super) struct PersistedStateRejectedRecord {
    evidence: PersistedStateRejectedRecordEvidence,
    identity: AnchoredRecordIdentity,
    restart_identity: RestartStableRecordIdentity,
}

impl PersistedStateRejectedRecord {
    pub(super) fn new(
        store: PersistedStateRecordStore,
        rejection: PersistedStateRecordRejection,
        target: TargetDescriptor,
        identity: AnchoredRecordIdentity,
        restart_digest: AnchoredRecordRestartDigest,
    ) -> Self {
        Self {
            evidence: PersistedStateRejectedRecordEvidence {
                store,
                rejection,
                target,
            },
            identity,
            restart_identity: RestartStableRecordIdentity::from_digest(restart_digest.into_bytes()),
        }
    }

    pub(super) fn evidence(&self) -> PersistedStateRejectedRecordEvidence {
        self.evidence.clone()
    }

    pub(super) fn store(&self) -> PersistedStateRecordStore {
        self.evidence.store
    }

    pub(super) fn record_id(&self) -> &str {
        &self.evidence.target.id
    }

    pub(super) fn restart_identity(&self) -> &RestartStableRecordIdentity {
        &self.restart_identity
    }

    pub(super) fn into_eligibility(self) -> PersistedStateRejectedRecordEligibility {
        PersistedStateRejectedRecordEligibility { record: self }
    }
}

pub(crate) struct PersistedStateRejectedRecordEligibility {
    record: PersistedStateRejectedRecord,
}

impl PersistedStateRejectedRecordEligibility {
    pub(super) fn still_current(&self) -> bool {
        self.record.identity.revalidate().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> PersistedStateRecordStore {
        self.record.store()
    }

    #[cfg(test)]
    pub(crate) fn record_id(&self) -> &str {
        self.record.record_id()
    }

    #[cfg(test)]
    pub(crate) fn physical_identity(&self) -> &RestartStableRecordIdentity {
        self.record.restart_identity()
    }
}

pub(super) struct PersistedStateRejectedRecordStoreScan {
    store: PersistedStateRecordStore,
    authoritative: bool,
    rejected_records: Vec<PersistedStateRejectedRecord>,
}

impl PersistedStateRejectedRecordStoreScan {
    pub(super) fn new(
        store: PersistedStateRecordStore,
        authoritative: bool,
        rejected_records: Vec<PersistedStateRejectedRecord>,
    ) -> Self {
        debug_assert!(
            rejected_records
                .iter()
                .all(|record| record.store() == store)
        );
        Self {
            store,
            authoritative,
            rejected_records,
        }
    }

    pub(super) fn evidence(
        &self,
    ) -> impl Iterator<Item = PersistedStateRejectedRecordEvidence> + '_ {
        self.rejected_records
            .iter()
            .map(PersistedStateRejectedRecord::evidence)
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        PersistedStateRecordStore,
        bool,
        Vec<PersistedStateRejectedRecord>,
    ) {
        (self.store, self.authoritative, self.rejected_records)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedStateLoadEvidence {
    issue_count: usize,
    rejected_records: Vec<PersistedStateRejectedRecordEvidence>,
}

impl PersistedStateLoadEvidence {
    pub(super) fn from_store_parts(
        issue_counts: [usize; 5],
        rejected_records: impl IntoIterator<Item = PersistedStateRejectedRecordEvidence>,
    ) -> Self {
        Self {
            issue_count: issue_counts.into_iter().fold(0usize, usize::saturating_add),
            rejected_records: rejected_records.into_iter().collect(),
        }
    }

    pub(crate) fn issue_count(&self) -> usize {
        self.issue_count
    }

    pub(crate) fn rejected_records(&self) -> &[PersistedStateRejectedRecordEvidence] {
        &self.rejected_records
    }

    #[cfg(test)]
    pub(crate) fn for_test(issue_count: usize) -> Self {
        Self::from_store_parts([issue_count, 0, 0, 0, 0], [])
    }
}

#[cfg(test)]
mod tests {
    use super::{PersistedStateLoadEvidence, PersistedStateRejectedRecord};
    use static_assertions::assert_not_impl_any;
    use std::path::Path;

    assert_not_impl_any!(
        PersistedStateRejectedRecord:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );
    assert_not_impl_any!(
        super::PersistedStateRejectedRecordEligibility:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );

    #[test]
    fn five_store_issue_count_saturates() {
        let evidence = PersistedStateLoadEvidence::from_store_parts(
            [usize::MAX - 1, 1, 1, usize::MAX, usize::MAX],
            [],
        );

        assert_eq!(evidence.issue_count(), usize::MAX);
        assert!(evidence.rejected_records().is_empty());
    }
}
