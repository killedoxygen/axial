use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use axial_config::{AppPaths, INSTANCE_REGISTRY_MAX_ENTRIES, Instance, is_canonical_instance_id};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const USER_MOD_WITNESS_SCHEMA: &str = "axial.guardian_user_mod_witnesses";
const USER_MOD_WITNESS_SCHEMA_VERSION: u32 = 1;
const USER_MOD_WITNESS_MAX_BYTES: usize = 2 * 1024 * 1024;
const USER_MOD_WITNESS_MAX_ENTRIES: usize = 1024;
const USER_MOD_WITNESS_MAX_RECORDS: usize = INSTANCE_REGISTRY_MAX_ENTRIES;
const USER_MOD_WITNESS_LOCK_INVARIANT: &str =
    "user mod witness lock poisoned; committed and persisted state may diverge";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserModWitnessEntry {
    pub(super) digest: String,
    pub(super) size: u64,
    pub(super) modified_at_ns: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserModWitnessRecord {
    instance_id: String,
    instance_created_at: String,
    entries: Vec<UserModWitnessEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserModWitnessSnapshot {
    schema: String,
    schema_version: u32,
    witnesses: Vec<UserModWitnessRecord>,
}

impl UserModWitnessSnapshot {
    fn empty() -> Self {
        Self {
            schema: USER_MOD_WITNESS_SCHEMA.to_string(),
            schema_version: USER_MOD_WITNESS_SCHEMA_VERSION,
            witnesses: Vec::new(),
        }
    }

    fn record(
        &self,
        instance_id: &str,
        instance_created_at: &str,
    ) -> Option<&UserModWitnessRecord> {
        self.witnesses
            .binary_search_by(|record| record.instance_id.as_str().cmp(instance_id))
            .ok()
            .and_then(|index| self.witnesses.get(index))
            .filter(|record| record.instance_created_at == instance_created_at)
    }
}

struct UserModWitnessState {
    visible: UserModWitnessSnapshot,
    retry_candidate: Option<(u64, UserModWitnessSnapshot)>,
    startup_cleanup_pending: bool,
}

struct UserModWitnessPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

struct PendingUserModWitnessCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: UserModWitnessSnapshot,
}

pub(super) struct UserModWitnessStore {
    state: Arc<Mutex<UserModWitnessState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    persistence: UserModWitnessPersistence,
}

impl UserModWitnessStore {
    pub(super) fn claim(
        paths: &AppPaths,
        registered: &[Instance],
        registry_authoritative: bool,
    ) -> io::Result<Self> {
        Self::claim_with_coordinator(
            paths,
            registered,
            registry_authoritative,
            PersistenceCoordinator::global(),
        )
    }

    fn claim_with_coordinator(
        paths: &AppPaths,
        registered: &[Instance],
        registry_authoritative: bool,
        persistence: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let path = paths.user_mod_witness_file().to_path_buf();
        let owner = persistence.claim_owner(&path).map_err(io::Error::from)?;
        let writer = owner
            .writer(&path, user_mod_witness_target())
            .map_err(io::Error::from)?;
        let registered = registered
            .iter()
            .map(|instance| (instance.id.as_str(), instance.created_at.as_str()))
            .collect::<BTreeMap<_, _>>();
        let loaded =
            load_user_mod_witness_snapshot(&path).unwrap_or_else(UserModWitnessSnapshot::empty);
        let mut visible = loaded.clone();
        if registry_authoritative {
            visible.witnesses.retain(|record| {
                registered
                    .get(record.instance_id.as_str())
                    .is_some_and(|created_at| *created_at == record.instance_created_at)
            });
        }
        let startup_cleanup_pending = visible != loaded;

        Ok(Self {
            state: Arc::new(Mutex::new(UserModWitnessState {
                visible,
                retry_candidate: None,
                startup_cleanup_pending,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            persistence: UserModWitnessPersistence { owner, writer },
        })
    }

    pub(super) fn baseline_matches(
        &self,
        instance_id: &str,
        instance_created_at: &str,
        observed: &[UserModWitnessEntry],
    ) -> Option<bool> {
        let state = self.state.lock().expect(USER_MOD_WITNESS_LOCK_INVARIANT);
        state
            .visible
            .record(instance_id, instance_created_at)
            .map(|record| record.entries == observed)
    }

    pub(super) async fn publish(
        &self,
        instance_id: String,
        instance_created_at: String,
        entries: Vec<UserModWitnessEntry>,
    ) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let mut candidate = self
            .state
            .lock()
            .expect(USER_MOD_WITNESS_LOCK_INVARIANT)
            .visible
            .clone();
        match candidate
            .witnesses
            .binary_search_by(|record| record.instance_id.cmp(&instance_id))
        {
            Ok(index) => {
                candidate.witnesses[index] = UserModWitnessRecord {
                    instance_id,
                    instance_created_at,
                    entries,
                };
            }
            Err(index) => candidate.witnesses.insert(
                index,
                UserModWitnessRecord {
                    instance_id,
                    instance_created_at,
                    entries,
                },
            ),
        }
        self.commit_holding_gate(candidate, mutation)
            .await
            .map(drop)
    }

    pub(super) async fn remove(&self, instance_id: &str) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let mut candidate = self
            .state
            .lock()
            .expect(USER_MOD_WITNESS_LOCK_INVARIANT)
            .visible
            .clone();
        let before = candidate.witnesses.len();
        candidate
            .witnesses
            .retain(|record| record.instance_id != instance_id);
        if candidate.witnesses.len() == before {
            drop(mutation);
            return Ok(());
        }
        self.commit_holding_gate(candidate, mutation)
            .await
            .map(drop)
    }

    pub(super) async fn close(&self) -> io::Result<()> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let cleanup = {
            let state = self.state.lock().expect(USER_MOD_WITNESS_LOCK_INVARIANT);
            state.startup_cleanup_pending.then(|| state.visible.clone())
        };
        let mutation = match cleanup {
            Some(candidate) => self.commit_holding_gate(candidate, mutation).await?,
            None => mutation,
        };
        self.persistence
            .owner
            .close()
            .await
            .map_err(io::Error::from)?;
        drop(mutation);
        Ok(())
    }

    async fn reconcile_retry_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let retained = self
            .state
            .lock()
            .expect(USER_MOD_WITNESS_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(mutation);
        };
        let ticket = self.persistence.writer.retry().map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "user mod witness retry revision diverged from its exact candidate"
        );
        self.await_commit_holding_gate(
            PendingUserModWitnessCommit {
                ticket,
                revision,
                candidate,
            },
            mutation,
        )
        .await
    }

    async fn commit_holding_gate(
        &self,
        candidate: UserModWitnessSnapshot,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let encoding_candidate = candidate.clone();
        let encoded = tokio::task::spawn_blocking(move || {
            encode_user_mod_witness_snapshot(encoding_candidate)
        })
        .await
        .map_err(|_| io::Error::other("user mod witness encoder stopped"))??;
        let ticket = self
            .persistence
            .writer
            .accept(encoded, WriteUrgency::Immediate, Ok)
            .map_err(io::Error::from)?;
        let revision = ticket.revision().get();
        self.await_commit_holding_gate(
            PendingUserModWitnessCommit {
                ticket,
                revision,
                candidate,
            },
            mutation,
        )
        .await
    }

    async fn await_commit_holding_gate(
        &self,
        commit: PendingUserModWitnessCommit,
        mutation: OwnedMutexGuard<()>,
    ) -> io::Result<OwnedMutexGuard<()>> {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(USER_MOD_WITNESS_LOCK_INVARIANT);
                    state.visible = commit.candidate;
                    state.startup_cleanup_pending = false;
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(revision, _)| *revision == commit.revision)
                    {
                        state.retry_candidate = None;
                    }
                    Ok(())
                }
                Err(error) => {
                    if matches!(&error, PersistenceError::Write { .. }) {
                        state
                            .lock()
                            .expect(USER_MOD_WITNESS_LOCK_INVARIANT)
                            .retry_candidate = Some((commit.revision, commit.candidate));
                    }
                    Err(io::Error::from(error))
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx
            .await
            .map_err(|_| io::Error::other("user mod witness commit observer stopped"))?;
        result?;
        Ok(mutation)
    }
}

fn load_user_mod_witness_snapshot(path: &Path) -> Option<UserModWitnessSnapshot> {
    let mut file = File::open(path).ok()?;
    let mut data = Vec::new();
    file.by_ref()
        .take((USER_MOD_WITNESS_MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > USER_MOD_WITNESS_MAX_BYTES {
        return None;
    }
    let snapshot = serde_json::from_slice::<UserModWitnessSnapshot>(&data).ok()?;
    validate_user_mod_witness_snapshot(&snapshot).then_some(snapshot)
}

fn validate_user_mod_witness_snapshot(snapshot: &UserModWitnessSnapshot) -> bool {
    if snapshot.schema != USER_MOD_WITNESS_SCHEMA
        || snapshot.schema_version != USER_MOD_WITNESS_SCHEMA_VERSION
        || snapshot.witnesses.len() > USER_MOD_WITNESS_MAX_RECORDS
    {
        return false;
    }
    let mut previous_instance_id = None;
    for record in &snapshot.witnesses {
        if !is_canonical_instance_id(&record.instance_id)
            || record.instance_created_at.len() > 64
            || chrono::DateTime::parse_from_rfc3339(&record.instance_created_at).is_err()
            || previous_instance_id.is_some_and(|previous| previous >= record.instance_id.as_str())
            || record.entries.len() > USER_MOD_WITNESS_MAX_ENTRIES
        {
            return false;
        }
        previous_instance_id = Some(record.instance_id.as_str());
        if record.entries.iter().any(|entry| {
            entry.digest.len() != 64
                || !entry
                    .digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) || record
            .entries
            .windows(2)
            .any(|pair| witness_entry_key(&pair[0]) > witness_entry_key(&pair[1]))
        {
            return false;
        }
    }
    true
}

fn encode_user_mod_witness_snapshot(snapshot: UserModWitnessSnapshot) -> io::Result<Vec<u8>> {
    if !validate_user_mod_witness_snapshot(&snapshot) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "user mod witness snapshot is not canonical",
        ));
    }
    let encoded = serde_json::to_vec_pretty(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if encoded.len() > USER_MOD_WITNESS_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "user mod witness snapshot exceeds its byte budget",
        ));
    }
    Ok(encoded)
}

fn witness_entry_key(entry: &UserModWitnessEntry) -> (&str, u64, u64) {
    (&entry.digest, entry.size, entry.modified_at_ns)
}

fn user_mod_witness_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "guardian_user_mod_witnesses",
        OwnershipClass::LauncherManaged,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct FailOnceFileBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
    }

    impl FailOnceFileBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl AtomicWriteBackend for FailOnceFileBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected user mod witness write failure"));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, contents)
        }
    }

    struct TestPaths {
        root: PathBuf,
        paths: AppPaths,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "axial-user-mod-store-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create app root");
            Self {
                paths: AppPaths::from_root(root.to_path_buf()).expect("absolute test app root"),
                root,
            }
        }

        fn snapshot_path(&self) -> PathBuf {
            self.paths.user_mod_witness_file().to_path_buf()
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn instance(id: &str) -> Instance {
        crate::state::new_instance(
            id.to_string(),
            "Witness".to_string(),
            "1.21.1".to_string(),
            String::new(),
            String::new(),
        )
    }

    fn entry(byte: char, size: u64, modified_at_ns: u64) -> UserModWitnessEntry {
        UserModWitnessEntry {
            digest: std::iter::repeat_n(byte, 64).collect(),
            size,
            modified_at_ns,
        }
    }

    #[tokio::test]
    async fn baseline_is_persisted_reloaded_and_removed_without_user_details() {
        let fixture = TestPaths::new("roundtrip");
        let instance = instance("0000000000000001");
        let entries = vec![entry('a', 11, 22), entry('b', 33, 44)];
        let store =
            UserModWitnessStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim witness store");
        store
            .publish(
                instance.id.clone(),
                instance.created_at.clone(),
                entries.clone(),
            )
            .await
            .expect("publish baseline");
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &entries),
            Some(true)
        );
        store.close().await.expect("close first store");
        drop(store);

        let encoded = fs::read_to_string(fixture.snapshot_path()).expect("read strict snapshot");
        assert!(!encoded.contains("filename"));
        assert!(!encoded.contains("path"));
        assert!(!encoded.contains("content"));
        assert!(!encoded.contains("private.jar"));

        let reloaded =
            UserModWitnessStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("reload witness store");
        assert_eq!(
            reloaded.baseline_matches(&instance.id, &instance.created_at, &entries),
            Some(true)
        );
        reloaded
            .remove(&instance.id)
            .await
            .expect("remove retired baseline");
        assert_eq!(
            reloaded.baseline_matches(&instance.id, &instance.created_at, &entries),
            None
        );
        reloaded.close().await.expect("close reloaded store");
    }

    #[tokio::test]
    async fn restart_cleanup_removes_absent_and_stale_incarnations() {
        let fixture = TestPaths::new("cleanup");
        let current = instance("0000000000000002");
        let stale = instance("0000000000000003");
        let store =
            UserModWitnessStore::claim(&fixture.paths, &[current.clone(), stale.clone()], true)
                .expect("claim initial store");
        store
            .publish(
                current.id.clone(),
                current.created_at.clone(),
                vec![entry('a', 1, 1)],
            )
            .await
            .expect("publish current");
        store
            .publish(
                stale.id.clone(),
                stale.created_at.clone(),
                vec![entry('b', 2, 2)],
            )
            .await
            .expect("publish stale");
        store.close().await.expect("close initial store");
        drop(store);

        let mut replacement = current.clone();
        replacement.created_at = "2026-01-01T00:00:00Z".to_string();
        let cleaned = UserModWitnessStore::claim(&fixture.paths, &[replacement.clone()], true)
            .expect("claim cleaned store");
        assert_eq!(
            cleaned.baseline_matches(&current.id, &current.created_at, &[entry('a', 1, 1)]),
            None
        );
        cleaned.close().await.expect("persist restart cleanup");
        drop(cleaned);

        let reloaded = UserModWitnessStore::claim(&fixture.paths, &[replacement], true)
            .expect("reload cleaned store");
        assert!(
            reloaded
                .state
                .lock()
                .expect(USER_MOD_WITNESS_LOCK_INVARIANT)
                .visible
                .witnesses
                .is_empty()
        );
        reloaded.close().await.expect("close final store");
    }

    #[tokio::test]
    async fn rejected_registry_fallback_does_not_retire_witnesses() {
        let fixture = TestPaths::new("non-authoritative-registry");
        let instance = instance("0000000000000004");
        let entries = vec![entry('c', 3, 4)];
        let store =
            UserModWitnessStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim initial store");
        store
            .publish(
                instance.id.clone(),
                instance.created_at.clone(),
                entries.clone(),
            )
            .await
            .expect("publish baseline");
        store.close().await.expect("close initial store");
        drop(store);

        let preserved = UserModWitnessStore::claim(&fixture.paths, &[], false)
            .expect("claim with rejected registry fallback");
        assert_eq!(
            preserved.baseline_matches(&instance.id, &instance.created_at, &entries),
            Some(true)
        );
        preserved.close().await.expect("close preserved store");
    }

    #[tokio::test]
    async fn rejected_encode_bound_does_not_wedge_a_later_commit() {
        let fixture = TestPaths::new("encode-bound");
        let instance = instance("0000000000000005");
        let store =
            UserModWitnessStore::claim(&fixture.paths, std::slice::from_ref(&instance), true)
                .expect("claim witness store");
        let original = vec![entry('c', 1, 1)];
        store
            .publish(
                instance.id.clone(),
                instance.created_at.clone(),
                original.clone(),
            )
            .await
            .expect("publish original baseline");
        let error = store
            .publish(
                instance.id.clone(),
                instance.created_at.clone(),
                vec![entry('d', 1, 1); USER_MOD_WITNESS_MAX_ENTRIES + 1],
            )
            .await
            .expect_err("reject oversized witness before acceptance");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &original),
            Some(true)
        );

        let entries = vec![entry('e', 2, 3)];
        store
            .publish(
                instance.id.clone(),
                instance.created_at.clone(),
                entries.clone(),
            )
            .await
            .expect("later valid witness persists");
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &entries),
            Some(true)
        );
        store
            .remove(&instance.id)
            .await
            .expect("removal remains available after bound rejection");
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &entries),
            None
        );
        store.close().await.expect("close remains available");
    }

    #[tokio::test]
    async fn close_retries_the_exact_failed_write_candidate() {
        let fixture = TestPaths::new("write-retry-close");
        let instance = instance("0000000000000006");
        let entries = vec![entry('f', 5, 8)];
        let backend = Arc::new(FailOnceFileBackend::new());
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let store = UserModWitnessStore::claim_with_coordinator(
            &fixture.paths,
            std::slice::from_ref(&instance),
            true,
            coordinator,
        )
        .expect("claim controlled witness store");
        backend.fail_next();

        assert!(
            store
                .publish(
                    instance.id.clone(),
                    instance.created_at.clone(),
                    entries.clone(),
                )
                .await
                .is_err()
        );
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &entries),
            None
        );
        store.close().await.expect("close retries failed witness");
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            store.baseline_matches(&instance.id, &instance.created_at, &entries),
            Some(true)
        );
        assert_eq!(
            load_user_mod_witness_snapshot(&fixture.snapshot_path()).and_then(|snapshot| {
                snapshot
                    .record(&instance.id, &instance.created_at)
                    .map(|record| record.entries.clone())
            }),
            Some(entries)
        );
    }

    #[test]
    fn strict_reader_rejects_legacy_unknown_and_noncanonical_snapshots() {
        let fixture = TestPaths::new("strict");
        let path = fixture.snapshot_path();
        let invalid = [
            serde_json::json!({
                "schema": USER_MOD_WITNESS_SCHEMA,
                "schema_version": 0,
                "witnesses": []
            }),
            serde_json::json!({
                "schema": USER_MOD_WITNESS_SCHEMA,
                "schema_version": USER_MOD_WITNESS_SCHEMA_VERSION,
                "witnesses": [],
                "legacy": true
            }),
            serde_json::json!({
                "schema": USER_MOD_WITNESS_SCHEMA,
                "schema_version": USER_MOD_WITNESS_SCHEMA_VERSION,
                "witnesses": [{
                    "instance_id": "0000000000000001",
                    "instance_created_at": "2026-01-01T00:00:00Z",
                    "entries": [{
                        "digest": "A".repeat(64),
                        "size": 1,
                        "modified_at_ns": 1,
                        "filename": "private.jar"
                    }]
                }]
            }),
        ];

        for snapshot in invalid {
            fs::write(
                &path,
                serde_json::to_vec(&snapshot).expect("encode invalid snapshot"),
            )
            .expect("write invalid snapshot");
            assert!(load_user_mod_witness_snapshot(&path).is_none());
        }
    }

    #[test]
    fn strict_snapshot_enforces_record_and_entry_bounds() {
        let record = UserModWitnessRecord {
            instance_id: "0000000000000001".to_string(),
            instance_created_at: "2026-01-01T00:00:00Z".to_string(),
            entries: vec![entry('a', 1, 1); USER_MOD_WITNESS_MAX_ENTRIES + 1],
        };
        let oversized_entries = UserModWitnessSnapshot {
            schema: USER_MOD_WITNESS_SCHEMA.to_string(),
            schema_version: USER_MOD_WITNESS_SCHEMA_VERSION,
            witnesses: vec![record],
        };
        assert!(!validate_user_mod_witness_snapshot(&oversized_entries));

        let witnesses = (0..=USER_MOD_WITNESS_MAX_RECORDS)
            .map(|index| UserModWitnessRecord {
                instance_id: format!("{index:016x}"),
                instance_created_at: "2026-01-01T00:00:00Z".to_string(),
                entries: Vec::new(),
            })
            .collect();
        let oversized_records = UserModWitnessSnapshot {
            schema: USER_MOD_WITNESS_SCHEMA.to_string(),
            schema_version: USER_MOD_WITNESS_SCHEMA_VERSION,
            witnesses,
        };
        assert!(!validate_user_mod_witness_snapshot(&oversized_records));
    }
}
