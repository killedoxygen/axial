use crate::execution::anchored_record::{
    AnchoredRecordDirectory, AnchoredRecordIdentity, AnchoredRecordQuarantineError,
    AnchoredRecordQuarantinePreservationError, AnchoredRecordQuarantineReceipt,
    AnchoredRecordRestartContext, AnchoredRecordRestartDigest, anchored_record_quarantine_name,
};
use crate::state::contracts::{
    OperationId, OwnershipClass, PersistedStateRecordStore, RestartStableRecordIdentity,
    StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use axial_fs::LeafName;
use std::sync::Arc;
use std::io;
#[cfg(test)]
use std::path::PathBuf;

pub(super) const MAX_REJECTED_RESTART_RECORDS_PER_STORE: usize = 8;
pub(super) const MAX_RESTART_RECORD_BYTES: u64 = 256 * 1024;

pub(crate) fn persisted_state_load_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "persisted-state-load",
        OwnershipClass::LauncherManaged,
    )
}

pub(super) fn persisted_state_record_target(
    store: PersistedStateRecordStore,
    record_id: &str,
) -> TargetDescriptor {
    let artifact = match store {
        PersistedStateRecordStore::PerformanceOperation => {
            CurrentArtifact::PerformanceOperationStatus
        }
        PersistedStateRecordStore::BenchmarkSuiteDriver => {
            CurrentArtifact::BenchmarkSuiteDriverStatus
        }
    };
    let mut target = classify_current_artifact(artifact, record_id).target;
    target.id = record_id.to_string();
    target
}

#[cfg(test)]
pub(super) fn persisted_state_record_path(
    paths: &axial_config::AppPaths,
    store: PersistedStateRecordStore,
    record_id: &str,
) -> PathBuf {
    match store {
        PersistedStateRecordStore::PerformanceOperation => {
            super::performance_operations::operation_path(
                &super::performance_operations::operation_dir(paths),
                record_id,
            )
        }
        PersistedStateRecordStore::BenchmarkSuiteDriver => {
            super::benchmark_suite_drivers::driver_path(
                &super::benchmark_suite_drivers::driver_dir(paths),
                record_id,
            )
        }
    }
}

pub(super) fn persisted_state_record_name(
    store: PersistedStateRecordStore,
    record_id: &str,
) -> io::Result<LeafName> {
    let name = match store {
        PersistedStateRecordStore::PerformanceOperation => {
            let operation_id = OperationId::try_from(record_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "persisted performance operation id is not canonical",
                )
            })?;
            format!("{operation_id}.json")
        }
        PersistedStateRecordStore::BenchmarkSuiteDriver => {
            if !super::benchmark_suite_drivers::is_safe_driver_id(record_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "persisted benchmark suite driver id is not canonical",
                ));
            }
            format!("{record_id}.json")
        }
    };
    LeafName::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted-state record name is not a direct native leaf",
        )
    })
}

pub(super) fn admit_exact_applied_persisted_state_quarantine(
    directory: &AnchoredRecordDirectory,
    original_leaf: LeafName,
    attempt: &super::contracts::PersistedStateRepairAttempt,
) -> io::Result<Option<PersistedStateRejectedRecordQuarantineReceipt>> {
    match directory.read(original_leaf.as_os_str(), MAX_RESTART_RECORD_BYTES) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let suffix = super::contracts::persisted_state_repair_quarantine_suffix(attempt);
    let destination_name = anchored_record_quarantine_name(original_leaf.as_os_str(), suffix);
    let destination =
        match directory.read(destination_name.as_os_str(), MAX_RESTART_RECORD_BYTES) {
            Ok(destination) => destination,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    let (identity, digest) = match destination.into_restart_identity(
        restart_context(attempt.store()),
        &original_leaf,
    ) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(None),
        Err(error) => return Err(error),
    };
    if &RestartStableRecordIdentity::from_digest(digest.into_bytes())
        != attempt.physical_identity()
    {
        return Ok(None);
    }
    identity
        .admit_existing_quarantine(original_leaf.as_os_str())
        .map(|exact| Some(PersistedStateRejectedRecordQuarantineReceipt { exact }))
}

pub(super) fn restart_context(
    store: PersistedStateRecordStore,
) -> AnchoredRecordRestartContext {
    match store {
        PersistedStateRecordStore::PerformanceOperation => {
            AnchoredRecordRestartContext::PerformanceOperation
        }
        PersistedStateRecordStore::BenchmarkSuiteDriver => {
            AnchoredRecordRestartContext::BenchmarkSuiteDriver
        }
    }
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
    #[cfg(test)]
    pub(crate) fn store(&self) -> PersistedStateRecordStore {
        self.store
    }

    #[cfg(test)]
    pub(crate) fn rejection(&self) -> PersistedStateRecordRejection {
        self.rejection
    }

    #[cfg(test)]
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

    pub(super) fn into_eligibility(
        self,
        owner: Arc<()>,
    ) -> PersistedStateRejectedRecordEligibility {
        PersistedStateRejectedRecordEligibility {
            record: self,
            owner,
        }
    }
}

pub(crate) struct PersistedStateRejectedRecordEligibility {
    record: PersistedStateRejectedRecord,
    owner: Arc<()>,
}

#[must_use = "persisted-state quarantine receipt must be acknowledged or retained"]
pub(crate) struct PersistedStateRejectedRecordQuarantineReceipt {
    exact: AnchoredRecordQuarantineReceipt,
}

impl PersistedStateRejectedRecordEligibility {
    #[cfg(test)]
    pub(super) fn bind_owner_for_test(mut self, owner: Arc<()>) -> Self {
        self.owner = owner;
        self
    }

    pub(super) fn still_current(&self) -> bool {
        self.record.identity.revalidate().is_ok()
    }

    pub(super) fn quarantine(
        self,
        suffix: [u8; 16],
    ) -> Result<PersistedStateRejectedRecordQuarantineReceipt, AnchoredRecordQuarantineError> {
        let PersistedStateRejectedRecord { identity, .. } = self.record;
        identity
            .quarantine(suffix)
            .map(|exact| PersistedStateRejectedRecordQuarantineReceipt { exact })
    }

    pub(super) fn store(&self) -> PersistedStateRecordStore {
        self.record.store()
    }

    pub(super) fn record_id(&self) -> &str {
        self.record.record_id()
    }

    pub(super) fn physical_identity(&self) -> &RestartStableRecordIdentity {
        self.record.restart_identity()
    }

    pub(super) fn record_target(&self) -> &TargetDescriptor {
        &self.record.evidence.target
    }

    pub(super) fn belongs_to(&self, owner: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
}

#[cfg(test)]
pub(crate) fn persisted_state_rejected_record_eligibility_for_test(
    root: &std::path::Path,
    file_name: &std::ffi::OsStr,
    record_id: &str,
) -> std::io::Result<PersistedStateRejectedRecordEligibility> {
    let observation =
        crate::execution::anchored_record::AnchoredRecordDirectory::for_test_directory(root)?
        .read(file_name, MAX_RESTART_RECORD_BYTES)?;
    let canonical_leaf = LeafName::new(file_name.to_os_string()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "test record name is not a native leaf")
    })?;
    let (identity, restart_digest) = observation.into_restart_identity(
        AnchoredRecordRestartContext::PerformanceOperation,
        &canonical_leaf,
    )?;
    Ok(PersistedStateRejectedRecord::new(
        PersistedStateRecordStore::PerformanceOperation,
        PersistedStateRecordRejection::InvalidSchema,
        persisted_state_record_target(PersistedStateRecordStore::PerformanceOperation, record_id),
        identity,
        restart_digest,
    )
    .into_eligibility(Arc::new(())))
}

impl PersistedStateRejectedRecordQuarantineReceipt {
    pub(super) fn is_current(&self) -> bool {
        self.exact.is_current()
    }

    pub(super) fn acknowledge_preserved(
        self,
    ) -> Result<(), AnchoredRecordQuarantinePreservationError> {
        self.exact.acknowledge_preserved()
    }

    pub(super) fn acknowledge_applied_unverified(
        self,
    ) -> Option<AnchoredRecordQuarantinePreservationError> {
        self.exact.acknowledge_applied_unverified()
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
        issue_counts: [usize; 6],
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

    #[cfg(test)]
    pub(crate) fn rejected_records(&self) -> &[PersistedStateRejectedRecordEvidence] {
        &self.rejected_records
    }

    #[cfg(test)]
    pub(crate) fn for_test(issue_count: usize) -> Self {
        Self::from_store_parts([issue_count, 0, 0, 0, 0, 0], [])
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::persisted_state_record_target;
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
    assert_not_impl_any!(
        super::PersistedStateRejectedRecordQuarantineReceipt:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );

    #[test]
    fn six_store_issue_count_saturates() {
        let evidence = PersistedStateLoadEvidence::from_store_parts(
            [usize::MAX - 1, 1, 1, usize::MAX, usize::MAX, usize::MAX],
            [],
        );

        assert_eq!(evidence.issue_count(), usize::MAX);
        assert!(evidence.rejected_records().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn eligibility_consumes_into_destination_bound_quarantine_receipt() {
        if !cfg!(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        )) {
            return;
        }
        use super::PersistedStateRecordRejection;
        use crate::execution::anchored_record::AnchoredRecordDirectory;
        use crate::state::contracts::PersistedStateRecordStore;
        use std::ffi::OsStr;
        use std::fs;
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "axial-persisted-state-quarantine-delegation-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create rejected-record root");
        fs::write(root.join("record.json"), b"{").expect("write rejected record");
        let observation = AnchoredRecordDirectory::for_test_directory(&root)
            .expect("hold rejected-record directory")
            .read(OsStr::new("record.json"), 64)
            .expect("read rejected record");
        let (identity, restart_digest) = observation
            .into_restart_identity(
                super::AnchoredRecordRestartContext::PerformanceOperation,
                &axial_fs::LeafName::new("record.json").expect("test record leaf"),
            )
            .expect("seal rejected record identity");
        let eligibility = PersistedStateRejectedRecord::new(
            PersistedStateRecordStore::PerformanceOperation,
            PersistedStateRecordRejection::InvalidSchema,
            persisted_state_record_target(
                PersistedStateRecordStore::PerformanceOperation,
                "persisted-record-id",
            ),
            identity,
            restart_digest,
        )
        .into_eligibility(Arc::new(()));
        let receipt = match eligibility.quarantine([0x7a; 16]) {
            Ok(receipt) => receipt,
            Err(_) => panic!("consume exact quarantine eligibility"),
        };

        assert!(receipt.is_current());
        assert!(!root.join("record.json").exists());
        assert_eq!(
            fs::read(root.join(".record.json.axial-quarantine-7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a"))
                .expect("read quarantined record"),
            b"{"
        );
        let alias_path =
            root.join(".RECORD.JSON.AXIAL-QUARANTINE-7A7A7A7A7A7A7A7A7A7A7A7A7A7A7A7A");
        let mut alias = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(alias_path)
        {
            Ok(alias) => alias,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                receipt
                    .acknowledge_preserved()
                    .expect("settle quarantine on a case-insensitive filesystem");
                fs::remove_dir_all(&root).expect("remove rejected-record root");
                return;
            }
            Err(error) => panic!("inject post-park portable alias: {error}"),
        };
        std::io::Write::write_all(&mut alias, b"alias").expect("write portable alias");
        drop(alias);
        assert!(!receipt.is_current());
        let error = receipt
            .acknowledge_preserved()
            .expect_err("post-park alias must block preservation acknowledgement");
        drop(error);
        fs::remove_dir_all(&root).expect("remove rejected-record root");
    }
}
