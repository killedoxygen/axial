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
use axial_config::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

const REJECTION_STREAK_SCHEMA: &str = "axial.state.persisted_state_rejection_streaks.v1";
const REJECTION_STREAK_THRESHOLD: u8 = 3;
const MAX_REJECTION_STREAK_ENTRIES: usize = MAX_REJECTED_RESTART_RECORDS_PER_STORE * 2;
const MAX_REJECTION_STREAK_SNAPSHOT_BYTES: u64 = 32 * 1024;
const REJECTION_STREAK_FILE: &str = "persisted-state-rejection-streaks.json";
const REJECTION_STREAK_LOCK_INVARIANT: &str = "persisted-state rejection streak lock poisoned";
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
    snapshot_path: PathBuf,
    scans: Vec<PersistedStateRejectedRecordStoreScan>,
}

pub(super) struct PersistedStateRejectionStreaks {
    pending: Mutex<Option<PersistedStateRejectionStartup>>,
    eligibilities: Mutex<Option<Vec<PersistedStateRejectedRecordEligibility>>>,
}

impl PersistedStateRejectionStreaks {
    pub(super) fn new(paths: &AppPaths, scans: Vec<PersistedStateRejectedRecordStoreScan>) -> Self {
        Self {
            pending: Mutex::new(Some(PersistedStateRejectionStartup {
                snapshot_path: rejection_streak_path(paths),
                scans,
            })),
            eligibilities: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn discarded(scans: Vec<PersistedStateRejectedRecordStoreScan>) -> Self {
        drop(scans);
        Self {
            pending: Mutex::new(None),
            eligibilities: Mutex::new(None),
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

        let prepared = match tokio::task::spawn_blocking(move || {
            prepare_progression(startup, coordinator, encoder)
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
            mut eligibilities,
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
        let eligible_count = eligibilities.len();
        eligibilities.retain(PersistedStateRejectedRecordEligibility::still_current);
        if eligibilities.len() != eligible_count {
            warn!("a persisted-state rejection changed before eligibility publication");
        }

        *self
            .eligibilities
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT) = Some(eligibilities);
    }

    #[cfg(test)]
    pub(super) fn take_eligibilities(&self) -> Vec<PersistedStateRejectedRecordEligibility> {
        self.eligibilities
            .lock()
            .expect(REJECTION_STREAK_LOCK_INVARIANT)
            .take()
            .unwrap_or_default()
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
    coordinator: PersistenceCoordinator,
    encoder: SnapshotEncoder,
) -> PreparedProgression {
    let history = match read_history(&startup.snapshot_path) {
        Ok(history) => history,
        Err(error) => {
            error.warn();
            return PreparedProgression::Skipped;
        }
    };
    let (snapshot, eligibilities) = advance_snapshot(history, startup.scans);

    let owner = match coordinator.claim_owner(&startup.snapshot_path) {
        Ok(owner) => owner,
        Err(error) => {
            warn!(
                error_kind = ?error.io_kind(),
                "persisted-state rejection streak persistence claim failed"
            );
            return PreparedProgression::Skipped;
        }
    };
    let writer = match owner.writer(&startup.snapshot_path, rejection_streak_target()) {
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

fn read_history(
    snapshot_path: &Path,
) -> Result<PersistedStateRejectionStreakSnapshot, HistoryReadError> {
    let file = match File::open(snapshot_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistedStateRejectionStreakSnapshot {
                schema: REJECTION_STREAK_SCHEMA.to_string(),
                entries: Vec::new(),
            });
        }
        Err(error) => return Err(HistoryReadError::Io(error.kind())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_REJECTION_STREAK_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| HistoryReadError::Io(error.kind()))?;
    if bytes.len() as u64 > MAX_REJECTION_STREAK_SNAPSHOT_BYTES {
        return Err(HistoryReadError::Oversized);
    }
    let snapshot = serde_json::from_slice::<PersistedStateRejectionStreakSnapshot>(&bytes)
        .map_err(|_| HistoryReadError::Invalid)?;
    validate_snapshot(&snapshot)?;
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
                eligibilities.push(record.into_eligibility());
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
            super::performance_operations::is_safe_operation_id(record_id)
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

fn rejection_streak_path(paths: &AppPaths) -> PathBuf {
    paths.config_dir.join("state").join(REJECTION_STREAK_FILE)
}

fn rejection_streak_target() -> crate::state::contracts::TargetDescriptor {
    classify_current_artifact(
        CurrentArtifact::PersistedStateRejectionStreakSnapshot,
        "persisted_state_rejection_streaks",
    )
    .target
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
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)
        }
    }

    struct FailingBackend;

    impl AtomicWriteBackend for FailingBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
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
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.started.notify_one();
            let (released, changed) = &self.released;
            let mut released = released.lock().expect("blocking backend lock");
            while !*released {
                released = changed.wait(released).expect("blocking backend wait");
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)
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
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: config_dir.join("instances"),
            music_dir: config_dir.join("music"),
            library_dir: config_dir.join("library"),
            config_dir,
        }
    }

    fn performance_id(index: u128) -> String {
        format!("performance-install-{index:032x}")
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
        let directory =
            AnchoredRecordDirectory::open(&directory_path).expect("hold rejected record directory");
        let observation = directory
            .read_for_mutation(OsStr::new(leaf), MAX_RESTART_RECORD_BYTES)
            .expect("read rejected record");
        let (identity, restart_digest) = observation
            .into_restart_identity()
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
        read_history(&rejection_streak_path(paths))
            .unwrap_or_else(|_| panic!("read committed rejection streaks"))
    }

    fn test_coordinator(backend: Arc<dyn AtomicWriteBackend>) -> PersistenceCoordinator {
        PersistenceCoordinator::for_test(backend, Duration::ZERO, Duration::ZERO)
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
            read_history(&path).unwrap_or_else(|_| panic!("strict snapshot")),
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
                read_history(&path),
                Err(HistoryReadError::Invalid)
            ));
        }
        let duplicate_field = format!(
            "{{\"schema\":\"{REJECTION_STREAK_SCHEMA}\",\"schema\":\"{REJECTION_STREAK_SCHEMA}\",\"entries\":[]}}"
        );
        fs::write(&path, duplicate_field).expect("write duplicate field snapshot");
        assert!(matches!(
            read_history(&path),
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
            let streaks = Arc::new(PersistedStateRejectionStreaks::new(
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
        let streaks = Arc::new(PersistedStateRejectionStreaks::new(
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
            let streaks = PersistedStateRejectionStreaks::new(
                &paths,
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
                        .claim_owner(&snapshot_path)
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
                .claim_owner(&snapshot_path)
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
        let streaks = PersistedStateRejectionStreaks::new(&paths, Vec::new());
        let coordinator = test_coordinator(Arc::new(RecordingBackend));

        assert!(streaks.has_pending_startup());
        let owner = coordinator
            .claim_owner(&snapshot_path)
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
