use crate::execution::anchored_record::{AnchoredRecordDirectory, AnchoredRecordObservation};
use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceOwnerLease,
    WriteUrgency,
};
use crate::state::contracts::{PersistedStateRecordStore, RestartStableRecordIdentity};
use crate::state::ownership::{CurrentArtifact, classify_current_artifact};
use crate::state::persisted_state_load::{
    MAX_REJECTED_RESTART_RECORDS_PER_STORE, PersistedStateRejectedRecordEligibility,
    PersistedStateRejectedRecordStoreScan,
};
#[cfg(test)]
use axial_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::warn;

const REJECTION_STREAK_SCHEMA: &str = "axial.state.persisted_state_rejection_streaks.v1";
const REJECTION_STREAK_THRESHOLD: u8 = 3;
const MAX_REJECTION_STREAK_ENTRIES: usize = MAX_REJECTED_RESTART_RECORDS_PER_STORE * 2;
const MAX_REJECTION_STREAK_SNAPSHOT_BYTES: u64 = 32 * 1024;
const REJECTION_STREAK_LOCK_INVARIANT: &str = "persisted-state rejection streak lock poisoned";
const REJECTION_STREAK_SNAPSHOT_NAME: &str = "persisted-state-rejection-streaks.json";
type SnapshotEncoder = fn(PersistedStateRejectionStreakSnapshot) -> io::Result<Vec<u8>>;

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedStateRejectionStreakSnapshot {
    schema: String,
    entries: Vec<PersistedStateRejectionStreakEntry>,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedStateRejectionStreakEntry {
    store: PersistedStateRecordStore,
    record_id: String,
    physical_identity: RestartStableRecordIdentity,
    consecutive_startups: u8,
}

struct PersistedStateRejectionStartup {
    directory: AnchoredRecordDirectory,
    scans: Vec<PersistedStateRejectedRecordStoreScan>,
}

pub(super) struct PersistedStateRejectionStreaks {
    pending: Mutex<Option<PersistedStateRejectionStartup>>,
    eligibilities: Mutex<Option<Vec<PersistedStateRejectedRecordEligibility>>>,
    repair_owner: Arc<()>,
}

impl PersistedStateRejectionStreaks {
    pub(super) fn new(
        directory: AnchoredRecordDirectory,
        scans: Vec<PersistedStateRejectedRecordStoreScan>,
    ) -> Self {
        Self {
            pending: Mutex::new(Some(PersistedStateRejectionStartup {
                directory,
                scans,
            })),
            eligibilities: Mutex::new(None),
            repair_owner: Arc::new(()),
        }
    }

    #[cfg(test)]
    pub(super) fn discarded(scans: Vec<PersistedStateRejectedRecordStoreScan>) -> Self {
        drop(scans);
        Self {
            pending: Mutex::new(None),
            eligibilities: Mutex::new(None),
            repair_owner: Arc::new(()),
        }
    }

    pub(super) async fn progress_startup(&self) {
        self.progress_startup_with(PersistenceCoordinator::global(), encode_snapshot)
            .await;
    }

    async fn progress_startup_with(
        &self,
        coordinator: PersistenceCoordinator,
        encoder: SnapshotEncoder,
    ) {
        let Some(startup) = self
            .pending
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT)
            .take()
        else {
            return;
        };

        let repair_owner = self.repair_owner.clone();
        let prepared = match tokio::task::spawn_blocking(move || {
            prepare_progression(startup, repair_owner, coordinator, encoder)
        })
        .await
        {
            Ok(prepared) => prepared,
            Err(_) => {
                warn!("persisted-state rejection streak startup task stopped");
                return;
            }
        };

        let PreparedProgression::Commit {
            owner,
            writer,
            accepted,
            eligibilities,
        } = prepared
        else {
            return;
        };

        if let Err(error) = accepted.persisted().await {
            writer.wait_until_idle().await;
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak snapshot was not committed"
            );
            return;
        }
        if let Err(error) = owner.close().await {
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak persistence did not close cleanly"
            );
            return;
        }
        let revalidated = tokio::task::spawn_blocking(move || {
            let eligible_count = eligibilities.len();
            let mut eligibilities = eligibilities;
            eligibilities.retain(PersistedStateRejectedRecordEligibility::still_current);
            (eligibilities, eligible_count)
        })
        .await;
        let Ok((eligibilities, eligible_count)) = revalidated else {
            warn!("persisted-state rejection revalidation task stopped");
            return;
        };
        if eligibilities.len() != eligible_count {
            warn!("a persisted-state rejection changed before eligibility publication");
        }

        *self
            .eligibilities
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT) = Some(eligibilities);
    }

    pub(super) fn take_eligibilities(&self) -> Vec<PersistedStateRejectedRecordEligibility> {
        self.eligibilities
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT)
            .take()
            .unwrap_or_default()
    }

    pub(super) fn repair_owner(&self) -> &Arc<()> {
        &self.repair_owner
    }

    #[cfg(test)]
    pub(super) fn publish_eligibilities_for_test(
        &self,
        eligibilities: Vec<PersistedStateRejectedRecordEligibility>,
    ) {
        let eligibilities = eligibilities
            .into_iter()
            .map(|eligibility| eligibility.bind_owner_for_test(self.repair_owner.clone()))
            .collect();
        *self
            .eligibilities
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT) = Some(eligibilities);
    }

    #[cfg(test)]
    fn has_pending_startup(&self) -> bool {
        self.pending
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT)
            .is_some()
    }
}

enum PreparedProgression {
    Skipped,
    Commit {
        owner: PersistenceOwnerLease,
        writer: AtomicSnapshotWriter,
        accepted: AcceptedWrite,
        eligibilities: Vec<PersistedStateRejectedRecordEligibility>,
    },
}

fn prepare_progression(
    startup: PersistedStateRejectionStartup,
    repair_owner: Arc<()>,
    coordinator: PersistenceCoordinator,
    encoder: SnapshotEncoder,
) -> PreparedProgression {
    let history = match read_history_anchored(&startup.directory) {
        Ok(history) => history,
        Err(error) => {
            error.warn();
            return PreparedProgression::Skipped;
        }
    };
    let (snapshot, eligibilities) = advance_snapshot(history, startup.scans, &repair_owner);

    let record = match startup
        .directory
        .target(
            std::ffi::OsStr::new(REJECTION_STREAK_SNAPSHOT_NAME),
            MAX_REJECTION_STREAK_SNAPSHOT_BYTES,
        )
    {
        Ok(record) => record,
        Err(error) => {
            warn!(error_kind = ?error.kind(), "persisted-state rejection streak target claim failed");
            return PreparedProgression::Skipped;
        }
    };
    let owner = match coordinator.claim_record(record.clone()) {
        Ok(owner) => owner,
        Err(error) => {
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak persistence claim failed"
            );
            return PreparedProgression::Skipped;
        }
    };
    let writer = match owner.writer(record) {
        Ok(writer) => writer,
        Err(error) => {
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak snapshot claim failed"
            );
            return PreparedProgression::Skipped;
        }
    };
    let accepted = match writer.accept(snapshot, WriteUrgency::Immediate, encoder) {
        Ok(accepted) => accepted,
        Err(error) => {
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak snapshot was not accepted"
            );
            return PreparedProgression::Skipped;
        }
    };

    PreparedProgression::Commit {
        owner,
        writer,
        accepted,
        eligibilities,
    }
}

fn read_history_anchored(
    directory: &AnchoredRecordDirectory,
) -> Result<PersistedStateRejectionStreakSnapshot, HistoryReadError> {
    let observation = match directory.read(
        std::ffi::OsStr::new(REJECTION_STREAK_SNAPSHOT_NAME),
        MAX_REJECTION_STREAK_SNAPSHOT_BYTES,
    ) {
        Ok(observation @ AnchoredRecordObservation::Bytes { .. }) => observation,
        Ok(AnchoredRecordObservation::Oversized { .. }) => {
            return Err(HistoryReadError::Oversized);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistedStateRejectionStreakSnapshot {
                schema: REJECTION_STREAK_SCHEMA.to_string(),
                entries: Vec::new(),
            });
        }
        Err(error) => return Err(HistoryReadError::Io(error.kind())),
    };
    let snapshot = serde_json::from_slice::<PersistedStateRejectionStreakSnapshot>(
        observation
            .bytes()
            .expect("bounded rejection streak observation has bytes"),
    )
        .map_err(|_| HistoryReadError::Invalid)?;
    validate_snapshot(&snapshot)?;
    observation
        .admit(MAX_REJECTION_STREAK_SNAPSHOT_BYTES)
        .map_err(|error| HistoryReadError::Io(error.kind()))?;
    Ok(snapshot)
}

fn validate_snapshot(
    snapshot: &PersistedStateRejectionStreakSnapshot,
) -> Result<(), HistoryReadError> {
    if snapshot.schema != REJECTION_STREAK_SCHEMA
        || snapshot.entries.len() > MAX_REJECTION_STREAK_ENTRIES
    {
        return Err(HistoryReadError::Invalid);
    }
    let mut previous_record = None;
    let mut performance_entries = 0usize;
    let mut driver_entries = 0usize;
    for entry in &snapshot.entries {
        let record = (entry.store, entry.record_id.as_str());
        match entry.store {
            PersistedStateRecordStore::PerformanceOperation => {
                performance_entries = performance_entries.saturating_add(1)
            }
            PersistedStateRecordStore::BenchmarkSuiteDriver => {
                driver_entries = driver_entries.saturating_add(1)
            }
        }
        if !(1..=REJECTION_STREAK_THRESHOLD).contains(&entry.consecutive_startups)
            || !record_id_is_valid(entry.store, &entry.record_id)
            || previous_record.is_some_and(|previous| previous >= record)
            || performance_entries > MAX_REJECTED_RESTART_RECORDS_PER_STORE
            || driver_entries > MAX_REJECTED_RESTART_RECORDS_PER_STORE
        {
            return Err(HistoryReadError::Invalid);
        }
        previous_record = Some(record);
    }
    Ok(())
}

fn advance_snapshot(
    history: PersistedStateRejectionStreakSnapshot,
    scans: Vec<PersistedStateRejectedRecordStoreScan>,
    repair_owner: &Arc<()>,
) -> (
    PersistedStateRejectionStreakSnapshot,
    Vec<PersistedStateRejectedRecordEligibility>,
) {
    let mut history = history
        .entries
        .into_iter()
        .map(|entry| {
            (
                (entry.store, entry.record_id),
                (entry.physical_identity, entry.consecutive_startups),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = Vec::new();
    let mut eligibilities = Vec::new();

    for scan in scans {
        let (store, authoritative, records) = scan.into_parts();
        if !authoritative {
            continue;
        }
        for record in records {
            let record_id = record.record_id().to_string();
            let physical_identity = record.restart_identity().clone();
            let consecutive_startups = history
                .remove(&(store, record_id.clone()))
                .filter(|(identity, _)| identity == &physical_identity)
                .map_or(1, |(_, consecutive_startups)| {
                    consecutive_startups
                        .saturating_add(1)
                        .min(REJECTION_STREAK_THRESHOLD)
                });
            entries.push(PersistedStateRejectionStreakEntry {
                store,
                record_id,
                physical_identity,
                consecutive_startups,
            });
            if consecutive_startups == REJECTION_STREAK_THRESHOLD {
                eligibilities.push(record.into_eligibility(repair_owner.clone()));
            }
        }
    }
    entries.sort_by(|left, right| {
        (left.store, left.record_id.as_str()).cmp(&(right.store, right.record_id.as_str()))
    });
    debug_assert!(entries.len() <= MAX_REJECTION_STREAK_ENTRIES);

    (
        PersistedStateRejectionStreakSnapshot {
            schema: REJECTION_STREAK_SCHEMA.to_string(),
            entries,
        },
        eligibilities,
    )
}

fn record_id_is_valid(store: PersistedStateRecordStore, record_id: &str) -> bool {
    match store {
        PersistedStateRecordStore::PerformanceOperation => {
            super::contracts::OperationId::try_from(record_id).is_ok()
        }
        PersistedStateRecordStore::BenchmarkSuiteDriver => {
            super::benchmark_suite_drivers::is_safe_driver_id(record_id)
        }
    }
}

fn encode_snapshot(snapshot: PersistedStateRejectionStreakSnapshot) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
fn rejection_streak_path(paths: &AppPaths) -> PathBuf {
    paths
        .persisted_state_rejection_streaks_file()
        .to_path_buf()
}

enum HistoryReadError {
    Io(io::ErrorKind),
    Oversized,
    Invalid,
}

impl HistoryReadError {
    fn warn(self) {
        match self {
            Self::Io(error_kind) => warn!(
                ?error_kind,
                "persisted-state rejection streak history could not be read"
            ),
            Self::Oversized => warn!("persisted-state rejection streak history is oversized"),
            Self::Invalid => warn!("persisted-state rejection streak history is invalid"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::anchored_record::AnchoredRecordDirectory;
    use crate::execution::persistence::AtomicWriteBackend;
    use crate::state::contracts::TargetDescriptor;
    use crate::state::persisted_state_load::{
        MAX_RESTART_RECORD_BYTES, PersistedStateRecordRejection, PersistedStateRejectedRecord,
    };
    use std::ffi::OsStr;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar};
    use std::time::Duration;
    use tokio::sync::Notify;

    static TEST_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct RecordingBackend;

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            destination.write(effects, contents)
        }
    }

    struct FailingBackend;

    impl AtomicWriteBackend for FailingBackend {
        fn write(
            &self,
            _destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            _effects: &axial_fs::EffectOwner,
            _contents: &[u8],
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rejection streak write failure",
            ))
        }
    }

    struct BlockingBackend {
        started: Notify,
        released: (Mutex<bool>, Condvar),
    }

    impl BlockingBackend {
        fn new() -> Self {
            Self {
                started: Notify::new(),
                released: (Mutex::new(false), Condvar::new()),
            }
        }

        fn release(&self) {
            let (released, changed) = &self.released;
            *released.lock().expect("blocking backend lock") = true;
            changed.notify_one();
        }
    }

    impl AtomicWriteBackend for BlockingBackend {
        fn write(
            &self,
            destination: &crate::execution::anchored_record::AnchoredRecordTarget,
            effects: &axial_fs::EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            self.started.notify_one();
            let (released, changed) = &self.released;
            let mut released = released.lock().expect("blocking backend lock");
            while !*released {
                released = changed.wait(released).expect("blocking backend wait");
            }
            destination.write(effects, contents)
        }
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-rejection-streak-{label}-{}-{}",
            std::process::id(),
            TEST_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn performance_id(index: u128) -> String {
        super::super::contracts::OperationId::deterministic_test(format!("record-{index}"))
            .to_string()
    }

    fn driver_id(index: u64) -> String {
        format!("benchmark-suite-driver-{index:016x}")
    }

    fn test_record(
        root: &Path,
        leaf: &str,
        store: PersistedStateRecordStore,
        record_id: &str,
    ) -> PersistedStateRejectedRecord {
        let directory_path = root.join("records");
        fs::create_dir_all(&directory_path).expect("create rejected record directory");
        let record_path = directory_path.join(leaf);
        if !record_path.exists() {
            fs::write(&record_path, b"{").expect("write rejected record");
        }
        let directory = AnchoredRecordDirectory::for_test_directory(&directory_path)
            .expect("hold rejected record directory");
        let observation = directory
            .read(OsStr::new(leaf), MAX_RESTART_RECORD_BYTES)
            .expect("read rejected record");
        let (identity, restart_digest) = observation
            .into_restart_identity(
                crate::state::persisted_state_load::restart_context(store),
                &axial_fs::LeafName::new(leaf).expect("test rejected record leaf"),
            )
            .expect("derive rejected record identity");
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
        PersistedStateRejectedRecord::new(
            store,
            PersistedStateRecordRejection::InvalidSchema,
            target,
            identity,
            restart_digest,
        )
    }

    fn scan(
        store: PersistedStateRecordStore,
        authoritative: bool,
        records: Vec<PersistedStateRejectedRecord>,
    ) -> PersistedStateRejectedRecordStoreScan {
        PersistedStateRejectedRecordStoreScan::new(store, authoritative, records)
    }

    fn snapshot_entry(
        store: PersistedStateRecordStore,
        record_id: String,
        physical_identity: RestartStableRecordIdentity,
        consecutive_startups: u8,
    ) -> PersistedStateRejectionStreakEntry {
        PersistedStateRejectionStreakEntry {
            store,
            record_id,
            physical_identity,
            consecutive_startups,
        }
    }

    fn snapshot(
        entries: Vec<PersistedStateRejectionStreakEntry>,
    ) -> PersistedStateRejectionStreakSnapshot {
        PersistedStateRejectionStreakSnapshot {
            schema: REJECTION_STREAK_SCHEMA.to_string(),
            entries,
        }
    }

    fn write_snapshot(path: &Path, snapshot: &PersistedStateRejectionStreakSnapshot) -> Vec<u8> {
        let bytes = serde_json::to_vec_pretty(snapshot).expect("encode rejection streak snapshot");
        fs::create_dir_all(path.parent().expect("snapshot parent"))
            .expect("create snapshot parent");
        fs::write(path, &bytes).expect("write rejection streak snapshot");
        bytes
    }

    fn current_snapshot(paths: &AppPaths) -> PersistedStateRejectionStreakSnapshot {
        test_read_history(&rejection_streak_path(paths))
            .unwrap_or_else(|_| panic!("read committed rejection streaks"))
    }

    fn test_read_history(
        path: &Path,
    ) -> Result<PersistedStateRejectionStreakSnapshot, HistoryReadError> {
        let directory_path = path
            .parent()
            .ok_or(HistoryReadError::Io(io::ErrorKind::InvalidInput))?;
        let directory = AnchoredRecordDirectory::for_test_directory(directory_path)
            .map_err(|error| HistoryReadError::Io(error.kind()))?;
        read_history_anchored(&directory)
    }

    fn test_coordinator(backend: Arc<dyn AtomicWriteBackend>) -> PersistenceCoordinator {
        PersistenceCoordinator::for_test(backend, Duration::ZERO, Duration::ZERO)
    }

    fn streak_directory(paths: &AppPaths) -> AnchoredRecordDirectory {
        let root_session = Arc::new(paths.open_root_session().expect("open rejection streak root"));
        let directory = root_session
            .prepare_persisted_state_directories()
            .expect("prepare rejection streak directory")
            .operation_journal_parent();
        AnchoredRecordDirectory::from_directory(root_session, directory)
    }

    fn test_streaks(
        paths: &AppPaths,
        scans: Vec<PersistedStateRejectedRecordStoreScan>,
    ) -> PersistedStateRejectionStreaks {
        PersistedStateRejectionStreaks::new(paths, streak_directory(paths), scans)
    }

    fn streak_target(
        directory: &AnchoredRecordDirectory,
        snapshot_path: &Path,
    ) -> crate::execution::anchored_record::AnchoredRecordTarget {
        directory
            .target(
                snapshot_path.file_name().expect("rejection streak leaf"),
                MAX_REJECTION_STREAK_SNAPSHOT_BYTES,
            )
            .expect("claim rejection streak target")
    }

    fn failing_encode(_snapshot: PersistedStateRejectionStreakSnapshot) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "injected rejection streak serialization failure",
        ))
    }

    #[test]
    fn strict_v1_snapshot_round_trips_only_canonical_bounded_entries() {
        let root = test_root("strict-round-trip");
        let paths = test_paths(&root);
        let path = rejection_streak_path(&paths);
        let expected = snapshot(vec![
            snapshot_entry(
                PersistedStateRecordStore::PerformanceOperation,
                performance_id(1),
                RestartStableRecordIdentity::from_digest([1; 32]),
                1,
            ),
            snapshot_entry(
                PersistedStateRecordStore::BenchmarkSuiteDriver,
                driver_id(1),
                RestartStableRecordIdentity::from_digest([2; 32]),
                3,
            ),
        ]);
        let encoded = write_snapshot(&path, &expected);
        let exact = format!(
            concat!(
                "{{\n",
                "  \"schema\": \"{}\",\n",
                "  \"entries\": [\n",
                "    {{\n",
                "      \"store\": \"performance_operation\",\n",
                "      \"record_id\": \"{}\",\n",
                "      \"physical_identity\": \"{}\",\n",
                "      \"consecutive_startups\": 1\n",
                "    }},\n",
                "    {{\n",
                "      \"store\": \"benchmark_suite_driver\",\n",
                "      \"record_id\": \"{}\",\n",
                "      \"physical_identity\": \"{}\",\n",
                "      \"consecutive_startups\": 3\n",
                "    }}\n",
                "  ]\n",
                "}}"
            ),
            REJECTION_STREAK_SCHEMA,
            performance_id(1),
            "sha256.01010101.01010101.01010101.01010101.01010101.01010101.01010101.01010101",
            driver_id(1),
            "sha256.02020202.02020202.02020202.02020202.02020202.02020202.02020202.02020202",
        );

        assert_eq!(encoded, exact.as_bytes());
        assert_eq!(
            test_read_history(&path).unwrap_or_else(|_| panic!("strict snapshot")),
            expected
        );
        fs::remove_dir_all(root).expect("remove strict round-trip root");
    }

    #[test]
    fn strict_v1_rejects_schema_shape_identity_count_order_and_cardinality_drift() {
        let root = test_root("strict-rejections");
        let paths = test_paths(&root);
        let path = rejection_streak_path(&paths);
        let identity = serde_json::to_value(RestartStableRecordIdentity::from_digest([3; 32]))
            .expect("serialize identity");
        let valid_entry = serde_json::json!({
            "store": "performance_operation",
            "record_id": performance_id(1),
            "physical_identity": identity,
            "consecutive_startups": 1
        });
        let cases = vec![
            serde_json::json!({"schema": "axial.state.persisted_state_rejection_streaks.v2", "entries": [valid_entry.clone()]}),
            serde_json::json!({"schema_version": 1, "entries": [valid_entry.clone()]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [valid_entry.clone()], "legacy": true}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "records": []}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "performance_operation", "record_id": "../unsafe", "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 1}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "performance_operation", "record_id": performance_id(1), "physical_identity": "sha256.AAAAAAAA.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa.aaaaaaaa", "consecutive_startups": 1}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "performance_operation", "record_id": performance_id(1), "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 1, "legacy": true}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "retired_store", "record_id": performance_id(1), "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 1}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "performance_operation", "record_id": performance_id(1), "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 0}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [{"store": "performance_operation", "record_id": performance_id(1), "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 4}]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [valid_entry.clone(), valid_entry.clone()]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [
                {"store": "performance_operation", "record_id": performance_id(2), "physical_identity": RestartStableRecordIdentity::from_digest([3; 32]), "consecutive_startups": 1},
                valid_entry.clone()
            ]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": [
                {"store": "benchmark_suite_driver", "record_id": driver_id(1), "physical_identity": RestartStableRecordIdentity::from_digest([4; 32]), "consecutive_startups": 1},
                valid_entry.clone()
            ]}),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": (1_u128..=9).map(|index| serde_json::json!({
                "store": "performance_operation",
                "record_id": performance_id(index),
                "physical_identity": RestartStableRecordIdentity::from_digest([5; 32]),
                "consecutive_startups": 1
            })).collect::<Vec<_>>() }),
            serde_json::json!({"schema": REJECTION_STREAK_SCHEMA, "entries": (1_u128..=17).map(|index| serde_json::json!({
                "store": "performance_operation",
                "record_id": performance_id(index),
                "physical_identity": RestartStableRecordIdentity::from_digest([6; 32]),
                "consecutive_startups": 1
            })).collect::<Vec<_>>() }),
        ];

        fs::create_dir_all(path.parent().expect("snapshot parent")).expect("create snapshot root");
        for value in cases {
            fs::write(
                &path,
                serde_json::to_vec(&value).expect("encode invalid snapshot"),
            )
            .expect("write invalid snapshot");
            assert!(matches!(
                test_read_history(&path),
                Err(HistoryReadError::Invalid)
            ));
        }
        let duplicate_field = format!(
            "{{\"schema\":\"{REJECTION_STREAK_SCHEMA}\",\"schema\":\"{REJECTION_STREAK_SCHEMA}\",\"entries\":[]}}"
        );
        fs::write(&path, duplicate_field).expect("write duplicate field snapshot");
        assert!(matches!(
            test_read_history(&path),
            Err(HistoryReadError::Invalid)
        ));

        fs::remove_dir_all(root).expect("remove strict rejection root");
    }

    #[tokio::test]
    async fn exact_identity_progresses_one_two_three_and_saturates_after_commit() {
        let root = test_root("progression");
        let paths = test_paths(&root);
        let id = performance_id(1);

        for expected_count in [1, 2, 3, 3] {
            let record = test_record(
                &root,
                "performance.json",
                PersistedStateRecordStore::PerformanceOperation,
                &id,
            );
            let streaks = Arc::new(test_streaks(
                &paths,
                vec![scan(
                    PersistedStateRecordStore::PerformanceOperation,
                    true,
                    vec![record],
                )],
            ));
            assert!(streaks.take_eligibilities().is_empty());
            let clone = streaks.clone();
            tokio::join!(streaks.progress_startup(), clone.progress_startup());

            let committed = current_snapshot(&paths);
            assert_eq!(committed.entries.len(), 1);
            assert_eq!(committed.entries[0].consecutive_startups, expected_count);
            let eligibilities = clone.take_eligibilities();
            if expected_count < REJECTION_STREAK_THRESHOLD {
                assert!(eligibilities.is_empty());
            } else {
                assert_eq!(eligibilities.len(), 1);
                assert_eq!(
                    eligibilities[0].store(),
                    PersistedStateRecordStore::PerformanceOperation
                );
                assert_eq!(eligibilities[0].record_id(), id);
                assert_eq!(
                    eligibilities[0].physical_identity(),
                    &committed.entries[0].physical_identity
                );
            }
            assert!(clone.take_eligibilities().is_empty());
            drop(eligibilities);
            drop(clone);
            drop(streaks);
        }

        fs::remove_dir_all(root).expect("remove progression root");
    }

    #[tokio::test]
    async fn threshold_eligibility_is_hidden_until_the_exact_snapshot_write_finishes() {
        let root = test_root("commit-barrier");
        let paths = test_paths(&root);
        let snapshot_path = rejection_streak_path(&paths);
        let id = performance_id(1);
        let record = test_record(
            &root,
            "performance.json",
            PersistedStateRecordStore::PerformanceOperation,
            &id,
        );
        let prior_identity = record.restart_identity().clone();
        write_snapshot(
            &snapshot_path,
            &snapshot(vec![snapshot_entry(
                PersistedStateRecordStore::PerformanceOperation,
                id.clone(),
                prior_identity,
                2,
            )]),
        );
        let streaks = Arc::new(test_streaks(
            &paths,
            vec![scan(
                PersistedStateRecordStore::PerformanceOperation,
                true,
                vec![record],
            )],
        ));
        let backend = Arc::new(BlockingBackend::new());
        let coordinator = test_coordinator(backend.clone());
        let progressing = {
            let streaks = streaks.clone();
            tokio::spawn(async move {
                streaks
                    .progress_startup_with(coordinator, encode_snapshot)
                    .await;
            })
        };

        backend.started.notified().await;
        assert_eq!(current_snapshot(&paths).entries[0].consecutive_startups, 2);
        assert!(streaks.take_eligibilities().is_empty());
        backend.release();
        progressing.await.expect("progression task");

        assert_eq!(current_snapshot(&paths).entries[0].consecutive_startups, 3);
        assert_eq!(streaks.take_eligibilities().len(), 1);
        assert!(streaks.take_eligibilities().is_empty());
        drop(streaks);
        fs::remove_dir_all(root).expect("remove commit barrier root");
    }

    #[test]
    fn blind_store_resets_only_itself_while_other_store_reaches_threshold() {
        let root = test_root("blind-store");
        let performance_id = performance_id(1);
        let driver_id = driver_id(1);
        let performance = test_record(
            &root,
            "performance.json",
            PersistedStateRecordStore::PerformanceOperation,
            &performance_id,
        );
        let driver = test_record(
            &root,
            "driver.json",
            PersistedStateRecordStore::BenchmarkSuiteDriver,
            &driver_id,
        );
        let history = snapshot(vec![
            snapshot_entry(
                PersistedStateRecordStore::PerformanceOperation,
                performance_id.clone(),
                performance.restart_identity().clone(),
                2,
            ),
            snapshot_entry(
                PersistedStateRecordStore::BenchmarkSuiteDriver,
                driver_id.clone(),
                driver.restart_identity().clone(),
                2,
            ),
        ]);

        let (advanced, eligibilities) = advance_snapshot(
            history,
            vec![
                scan(
                    PersistedStateRecordStore::PerformanceOperation,
                    false,
                    vec![performance],
                ),
                scan(
                    PersistedStateRecordStore::BenchmarkSuiteDriver,
                    true,
                    vec![driver],
                ),
            ],
            &Arc::new(()),
        );

        assert_eq!(advanced.entries.len(), 1);
        assert_eq!(
            advanced.entries[0].store,
            PersistedStateRecordStore::BenchmarkSuiteDriver
        );
        assert_eq!(advanced.entries[0].consecutive_startups, 3);
        assert_eq!(eligibilities.len(), 1);
        assert_eq!(
            eligibilities[0].store(),
            PersistedStateRecordStore::BenchmarkSuiteDriver
        );
        drop(eligibilities);
        fs::remove_dir_all(root).expect("remove blind store root");
    }

    #[test]
    fn absence_and_physical_identity_replacement_restart_the_exact_streak() {
        let root = test_root("replacement-reset");
        let id = performance_id(1);
        let record = test_record(
            &root,
            "performance.json",
            PersistedStateRecordStore::PerformanceOperation,
            &id,
        );
        let replacement_history = snapshot(vec![snapshot_entry(
            PersistedStateRecordStore::PerformanceOperation,
            id.clone(),
            RestartStableRecordIdentity::from_digest([0xff; 32]),
            2,
        )]);

        let (replacement, eligibilities) = advance_snapshot(
            replacement_history,
            vec![scan(
                PersistedStateRecordStore::PerformanceOperation,
                true,
                vec![record],
            )],
            &Arc::new(()),
        );
        assert_eq!(replacement.entries.len(), 1);
        assert_eq!(replacement.entries[0].consecutive_startups, 1);
        assert!(eligibilities.is_empty());

        let (absent, eligibilities) = advance_snapshot(
            replacement,
            vec![scan(
                PersistedStateRecordStore::PerformanceOperation,
                true,
                Vec::new(),
            )],
            &Arc::new(()),
        );
        assert!(absent.entries.is_empty());
        assert!(eligibilities.is_empty());
        fs::remove_dir_all(root).expect("remove replacement root");
    }

    #[test]
    fn both_store_scans_retain_exactly_eight_entries_each() {
        let root = test_root("both-store-bound");
        let performance = (1_u128..=8)
            .map(|index| {
                let id = performance_id(index);
                test_record(
                    &root,
                    &format!("performance-{index}.json"),
                    PersistedStateRecordStore::PerformanceOperation,
                    &id,
                )
            })
            .collect::<Vec<_>>();
        let drivers = (1_u64..=8)
            .map(|index| {
                let id = driver_id(index);
                test_record(
                    &root,
                    &format!("driver-{index}.json"),
                    PersistedStateRecordStore::BenchmarkSuiteDriver,
                    &id,
                )
            })
            .collect::<Vec<_>>();

        let (advanced, eligibilities) = advance_snapshot(
            snapshot(Vec::new()),
            vec![
                scan(
                    PersistedStateRecordStore::PerformanceOperation,
                    true,
                    performance,
                ),
                scan(
                    PersistedStateRecordStore::BenchmarkSuiteDriver,
                    true,
                    drivers,
                ),
            ],
            &Arc::new(()),
        );

        assert_eq!(advanced.entries.len(), 16);
        assert_eq!(
            advanced
                .entries
                .iter()
                .filter(|entry| entry.store == PersistedStateRecordStore::PerformanceOperation)
                .count(),
            8
        );
        assert_eq!(
            advanced
                .entries
                .iter()
                .filter(|entry| entry.store == PersistedStateRecordStore::BenchmarkSuiteDriver)
                .count(),
            8
        );
        assert!(eligibilities.is_empty());
        fs::remove_dir_all(root).expect("remove both-store root");
    }

    #[tokio::test]
    async fn corrupt_claim_encode_and_write_failures_preserve_bytes_and_publish_nothing() {
        for failure in ["corrupt", "oversized", "claim", "encode", "write"] {
            let root = test_root(failure);
            let paths = test_paths(&root);
            let snapshot_path = rejection_streak_path(&paths);
            let id = performance_id(1);
            let record = test_record(
                &root,
                "performance.json",
                PersistedStateRecordStore::PerformanceOperation,
                &id,
            );
            let original = if matches!(failure, "corrupt" | "oversized") {
                let bytes = if failure == "oversized" {
                    vec![b'x'; MAX_REJECTION_STREAK_SNAPSHOT_BYTES as usize + 1]
                } else {
                    b"{not-json".to_vec()
                };
                fs::create_dir_all(snapshot_path.parent().expect("snapshot parent"))
                    .expect("create corrupt snapshot parent");
                fs::write(&snapshot_path, &bytes).expect("write corrupt snapshot");
                bytes
            } else {
                write_snapshot(
                    &snapshot_path,
                    &snapshot(vec![snapshot_entry(
                        PersistedStateRecordStore::PerformanceOperation,
                        id.clone(),
                        record.restart_identity().clone(),
                        2,
                    )]),
                )
            };
            let directory = streak_directory(&paths);
            let target = streak_target(&directory, &snapshot_path);
            let streaks = PersistedStateRejectionStreaks::new(
                &paths,
                directory,
                vec![scan(
                    PersistedStateRecordStore::PerformanceOperation,
                    true,
                    vec![record],
                )],
            );
            let backend: Arc<dyn AtomicWriteBackend> = if failure == "write" {
                Arc::new(FailingBackend)
            } else {
                Arc::new(RecordingBackend)
            };
            let coordinator = test_coordinator(backend);
            let reclaim_coordinator = coordinator.clone();
            let claim = if failure == "claim" {
                Some(
                    coordinator
                        .claim_record(target.clone())
                        .expect("hold conflicting snapshot claim"),
                )
            } else {
                None
            };
            let encoder: SnapshotEncoder = if failure == "encode" {
                failing_encode
            } else {
                encode_snapshot
            };

            streaks.progress_startup_with(coordinator, encoder).await;

            assert_eq!(
                fs::read(&snapshot_path).expect("read preserved snapshot"),
                original
            );
            assert!(streaks.take_eligibilities().is_empty());
            drop(claim);
            let reclaimed = reclaim_coordinator
                .claim_record(target)
                .unwrap_or_else(|error| {
                    panic!("{failure} one-shot owner was not immediately reclaimable: {error}")
                });
            drop(reclaimed);
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert_eq!(
                fs::read(&snapshot_path).expect("read snapshot after retry window"),
                original,
                "{failure} scheduled an unexpected retry"
            );
            drop(streaks);
            fs::remove_dir_all(root).expect("remove failure root");
        }
    }

    #[tokio::test]
    async fn construction_does_not_advance_or_claim_persistence() {
        let root = test_root("construction-only");
        let paths = test_paths(&root);
        let snapshot_path = rejection_streak_path(&paths);
        let directory = streak_directory(&paths);
        let target = streak_target(&directory, &snapshot_path);
        let streaks = PersistedStateRejectionStreaks::new(&paths, directory, Vec::new());
        let coordinator = test_coordinator(Arc::new(RecordingBackend));

        assert!(streaks.has_pending_startup());
        let owner = coordinator
            .claim_record(target)
            .expect("construction retained no persistence owner");
        assert!(!snapshot_path.exists());
        assert!(streaks.take_eligibilities().is_empty());

        drop(owner);
        drop(streaks);
        if root.exists() {
            fs::remove_dir_all(root).expect("remove construction root");
        }
    }

    #[tokio::test]
    async fn discarded_synchronous_bootstrap_releases_candidates_without_progression() {
        let root = test_root("discarded-bootstrap");
        let paths = test_paths(&root);
        let id = performance_id(1);
        let record = test_record(
            &root,
            "performance.json",
            PersistedStateRecordStore::PerformanceOperation,
            &id,
        );
        let streaks = PersistedStateRejectionStreaks::discarded(vec![scan(
            PersistedStateRecordStore::PerformanceOperation,
            true,
            vec![record],
        )]);

        assert!(!streaks.has_pending_startup());
        fs::rename(
            root.join("records").join("performance.json"),
            root.join("records").join("released.json"),
        )
        .expect("discard released anchored candidate");
        streaks.progress_startup().await;
        assert!(!rejection_streak_path(&paths).exists());
        assert!(streaks.take_eligibilities().is_empty());

        drop(streaks);
        fs::remove_dir_all(root).expect("remove discarded bootstrap root");
    }
}
