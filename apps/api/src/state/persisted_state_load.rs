use crate::execution::anchored_record::AnchoredRecordIdentity;
use crate::state::contracts::TargetDescriptor;

pub(super) const MAX_REJECTED_RESTART_RECORDS_PER_STORE: usize = 8;
pub(super) const MAX_RESTART_RECORD_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistedStateRecordStore {
    PerformanceOperation,
    BenchmarkSuiteDriver,
}

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
    _identity: AnchoredRecordIdentity,
}

impl PersistedStateRejectedRecord {
    pub(super) fn new(
        store: PersistedStateRecordStore,
        rejection: PersistedStateRecordRejection,
        target: TargetDescriptor,
        identity: AnchoredRecordIdentity,
    ) -> Self {
        Self {
            evidence: PersistedStateRejectedRecordEvidence {
                store,
                rejection,
                target,
            },
            _identity: identity,
        }
    }

    pub(super) fn evidence(&self) -> PersistedStateRejectedRecordEvidence {
        self.evidence.clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedStateLoadEvidence {
    issue_count: usize,
    rejected_records: Vec<PersistedStateRejectedRecordEvidence>,
}

impl PersistedStateLoadEvidence {
    fn from_store_parts(
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

pub(super) struct PersistedStateStartupLoad {
    evidence: PersistedStateLoadEvidence,
    _rejected_records: Vec<PersistedStateRejectedRecord>,
}

impl PersistedStateStartupLoad {
    pub(super) fn new(
        issue_counts: [usize; 5],
        rejected_records: Vec<PersistedStateRejectedRecord>,
    ) -> Self {
        let evidence = PersistedStateLoadEvidence::from_store_parts(
            issue_counts,
            rejected_records
                .iter()
                .map(PersistedStateRejectedRecord::evidence),
        );
        Self {
            evidence,
            _rejected_records: rejected_records,
        }
    }

    pub(super) fn evidence(&self) -> &PersistedStateLoadEvidence {
        &self.evidence
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PersistedStateLoadEvidence, PersistedStateRejectedRecord, PersistedStateStartupLoad,
    };
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
    assert_not_impl_any!(PersistedStateStartupLoad: Clone, serde::Serialize);

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
