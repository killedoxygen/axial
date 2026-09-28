use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex},
};

const MAX_KEYS_PER_LEASE: usize = 256;
const MAX_IDENTITY_BYTES: usize = 512;

/// Exact admitted root and artifact identities, never authority to open a path.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArtifactKey {
    root: String,
    artifact: String,
}

impl ArtifactKey {
    pub fn new(root: impl Into<String>, artifact: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            artifact: artifact.into(),
        }
    }
}

// Declaration order fixes target-before-artifact ordering. All reservations are
// taken atomically under one short lock; callers cannot wait with a partial set.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Key {
    Target(String),
    Artifact(ArtifactKey),
}

#[derive(Clone, Copy)]
enum ArtifactAccess {
    Read,
    Write,
}

enum Held {
    Exclusive,
    Readers(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExclusionError {
    InvalidIdentity,
    TooManyKeys,
    Busy,
}

impl fmt::Display for ExclusionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ExclusionError {}

/// Shared by all conflicting feature consumers in one application composition.
/// Acquire the library pin first, this complete reservation second, then short
/// metadata/filesystem locks. No mutex remains held during effects or I/O.
#[derive(Clone, Default)]
pub struct Exclusions {
    held: Arc<Mutex<BTreeMap<Key, Held>>>,
}

impl Exclusions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve exclusive targets and exclusive artifact mutation authority.
    pub fn try_acquire<I, S, A>(
        &self,
        targets: I,
        artifacts: A,
    ) -> Result<ExclusionLease, ExclusionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        A: IntoIterator<Item = ArtifactKey>,
    {
        self.acquire(targets, artifacts, ArtifactAccess::Write)
    }

    /// Reserve exclusive targets and shared artifact use. Independent sessions
    /// can retain the same artifacts, while their presence excludes writers.
    /// No reservation is published unless every target and artifact is compatible.
    pub fn try_acquire_read_artifacts<I, S, A>(
        &self,
        targets: I,
        artifacts: A,
    ) -> Result<ExclusionLease, ExclusionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        A: IntoIterator<Item = ArtifactKey>,
    {
        self.acquire(targets, artifacts, ArtifactAccess::Read)
    }

    fn acquire<I, S, A>(
        &self,
        targets: I,
        artifacts: A,
        access: ArtifactAccess,
    ) -> Result<ExclusionLease, ExclusionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        A: IntoIterator<Item = ArtifactKey>,
    {
        let mut keys = BTreeSet::new();
        let requested = targets
            .into_iter()
            .map(|target| Key::Target(target.into()))
            .chain(artifacts.into_iter().map(Key::Artifact));
        for (index, key) in requested.enumerate() {
            if index >= MAX_KEYS_PER_LEASE {
                return Err(ExclusionError::TooManyKeys);
            }
            let valid = match &key {
                Key::Target(target) => valid_identity(target),
                Key::Artifact(artifact) => {
                    valid_identity(&artifact.root) && valid_identity(&artifact.artifact)
                }
            };
            if !valid {
                return Err(ExclusionError::InvalidIdentity);
            }
            keys.insert(key);
        }
        let mut held = self.held.lock().expect("exclusion lock poisoned");
        if keys.iter().any(|key| match (key, access, held.get(key)) {
            (_, _, None) => false,
            (Key::Artifact(_), ArtifactAccess::Read, Some(Held::Readers(count))) => {
                count.checked_add(1).is_none()
            }
            _ => true,
        }) {
            return Err(ExclusionError::Busy);
        }
        for key in &keys {
            if matches!((key, access), (Key::Artifact(_), ArtifactAccess::Read)) {
                match held.entry(key.clone()).or_insert(Held::Readers(0)) {
                    // Overflow was checked for every key before any mutation.
                    Held::Readers(count) => *count += 1,
                    Held::Exclusive => unreachable!("artifact conflicts were checked"),
                }
            } else {
                held.insert(key.clone(), Held::Exclusive);
            }
        }
        Ok(ExclusionLease {
            reservation: Arc::new(Reservation {
                held: Arc::clone(&self.held),
                keys,
                access,
            }),
        })
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

struct Reservation {
    held: Arc<Mutex<BTreeMap<Key, Held>>>,
    keys: BTreeSet<Key>,
    access: ArtifactAccess,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut held = self.held.lock().expect("exclusion lock poisoned");
        for key in &self.keys {
            match held.get_mut(key) {
                Some(Held::Readers(count)) if *count > 1 => *count -= 1,
                _ => {
                    held.remove(key);
                }
            }
        }
    }
}

/// Clone only to transfer an existing obligation into a domain-owned guard or
/// receipt. Cloning does not grant another operation independent admission.
#[derive(Clone)]
pub struct ExclusionLease {
    reservation: Arc<Reservation>,
}

impl ExclusionLease {
    pub fn belongs_to(&self, exclusions: &Exclusions) -> bool {
        Arc::ptr_eq(&self.reservation.held, &exclusions.held)
    }

    pub fn covers_target(&self, target: &str) -> bool {
        self.reservation
            .keys
            .contains(&Key::Target(target.to_owned()))
    }

    /// Covers artifact use; this alone does not grant mutation authority.
    pub fn covers_artifact(&self, artifact: &ArtifactKey) -> bool {
        self.reservation
            .keys
            .contains(&Key::Artifact(artifact.clone()))
    }

    pub fn covers_artifact_write(&self, artifact: &ArtifactKey) -> bool {
        matches!(self.reservation.access, ArtifactAccess::Write) && self.covers_artifact(artifact)
    }

    pub fn target_count(&self) -> usize {
        self.reservation
            .keys
            .iter()
            .filter(|key| matches!(key, Key::Target(_)))
            .count()
    }

    pub fn artifact_count(&self) -> usize {
        self.reservation.keys.len() - self.target_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[test]
    fn same_target_conflicts_while_unrelated_targets_progress() {
        let exclusions = Exclusions::new();
        let first = exclusions.try_acquire(["instance-a"], []).unwrap();
        assert!(matches!(
            exclusions.try_acquire(["instance-a"], []),
            Err(ExclusionError::Busy)
        ));
        let other = exclusions.try_acquire(["instance-b"], []).unwrap();
        drop(first);
        assert!(exclusions.try_acquire(["instance-a"], []).is_ok());
        drop(other);
    }

    #[test]
    fn shared_artifact_conflict_never_partially_reserves_target() {
        let exclusions = Exclusions::new();
        let runtime = ArtifactKey::new("physical-library-1", "java-21");
        let first = exclusions
            .try_acquire(["instance-a"], [runtime.clone()])
            .unwrap();
        assert!(matches!(
            exclusions.try_acquire(["instance-b"], [runtime]),
            Err(ExclusionError::Busy)
        ));
        assert!(exclusions.try_acquire(["instance-b"], []).is_ok());
        assert!(
            exclusions
                .try_acquire(
                    std::iter::empty::<String>(),
                    [ArtifactKey::new("physical-library-2", "java-21")],
                )
                .is_ok()
        );
        drop(first);
    }

    #[test]
    fn escaped_receipt_keeps_every_reservation_until_settled() {
        let exclusions = Exclusions::new();
        let lease = exclusions
            .try_acquire(["b", "a", "a"], [ArtifactKey::new("root", "runtime")])
            .unwrap();
        assert_eq!(lease.target_count(), 2);
        assert_eq!(lease.artifact_count(), 1);
        let receipt = lease.clone();
        drop(lease);
        assert!(matches!(
            exclusions.try_acquire(["a", "b"], []),
            Err(ExclusionError::Busy)
        ));
        drop(receipt);
        assert!(exclusions.try_acquire(["a", "b"], []).is_ok());
    }

    #[test]
    fn invalid_or_unbounded_requests_reserve_nothing() {
        let exclusions = Exclusions::new();
        assert!(matches!(
            exclusions.try_acquire(["instance-a", ""], []),
            Err(ExclusionError::InvalidIdentity)
        ));
        assert!(matches!(
            exclusions.try_acquire(std::iter::repeat("same"), []),
            Err(ExclusionError::TooManyKeys)
        ));
        assert!(exclusions.try_acquire(["instance-a", "same"], []).is_ok());
    }

    #[test]
    fn coverage_rejects_mismatched_target_root_artifact_and_owner() {
        let exclusions = Exclusions::new();
        let runtime = ArtifactKey::new("physical-root", "java-21");
        let lease = exclusions
            .try_acquire(["instance-a"], [runtime.clone()])
            .unwrap();
        assert!(lease.belongs_to(&exclusions.clone()));
        assert!(!lease.belongs_to(&Exclusions::new()));
        assert!(lease.covers_target("instance-a"));
        assert!(!lease.covers_target("instance-b"));
        assert!(lease.covers_artifact(&runtime));
        assert!(lease.covers_artifact_write(&runtime));
        assert!(!lease.covers_artifact(&ArtifactKey::new("other-root", "java-21")));
        assert!(!lease.covers_artifact(&ArtifactKey::new("physical-root", "java-17")));
    }

    #[test]
    fn sessions_share_artifacts_but_keep_exclusive_targets_and_exclude_installers() {
        let exclusions = Exclusions::new();
        let artifacts = ArtifactKey::new("library", "managed-game-artifacts");
        let first = exclusions
            .try_acquire_read_artifacts(["instance-a"], [artifacts.clone()])
            .unwrap();
        let second = exclusions
            .try_acquire_read_artifacts(["instance-b"], [artifacts.clone()])
            .unwrap();
        assert!(first.covers_artifact(&artifacts));
        assert!(!first.covers_artifact_write(&artifacts));
        assert!(matches!(
            exclusions.try_acquire_read_artifacts(["instance-a"], []),
            Err(ExclusionError::Busy)
        ));
        assert!(matches!(
            exclusions.try_acquire(["instance-c"], [artifacts.clone()]),
            Err(ExclusionError::Busy)
        ));
        drop(first);
        assert!(matches!(
            exclusions.try_acquire(["instance-c"], [artifacts.clone()]),
            Err(ExclusionError::Busy)
        ));
        drop(second);
        let installer = exclusions
            .try_acquire(["instance-c"], [artifacts.clone()])
            .unwrap();
        assert!(matches!(
            exclusions.try_acquire_read_artifacts(["instance-a"], [artifacts]),
            Err(ExclusionError::Busy)
        ));
        assert!(exclusions.try_acquire(["instance-a"], []).is_ok());
        drop(installer);
    }

    #[test]
    fn rejected_multikey_reader_and_writer_do_not_publish_partial_reservations() {
        let exclusions = Exclusions::new();
        let shared = ArtifactKey::new("library", "a-shared");
        let exclusive = ArtifactKey::new("library", "z-exclusive");
        let reader = exclusions
            .try_acquire_read_artifacts(["reader"], [shared.clone()])
            .unwrap();
        let writer = exclusions
            .try_acquire(["writer"], [exclusive.clone()])
            .unwrap();
        assert!(matches!(
            exclusions
                .try_acquire_read_artifacts(["candidate"], [shared.clone(), exclusive.clone()]),
            Err(ExclusionError::Busy)
        ));
        assert!(exclusions.try_acquire(["candidate"], []).is_ok());
        drop(reader);
        assert!(exclusions.try_acquire(["reader"], [shared.clone()]).is_ok());

        let reader = exclusions
            .try_acquire_read_artifacts(["reader"], [shared.clone()])
            .unwrap();
        let untouched = ArtifactKey::new("library", "0-untouched");
        assert!(matches!(
            exclusions.try_acquire(["candidate"], [untouched.clone(), shared]),
            Err(ExclusionError::Busy)
        ));
        assert!(exclusions.try_acquire(["candidate"], [untouched]).is_ok());
        drop((reader, writer));
    }

    #[test]
    fn cloned_read_receipts_release_only_their_final_reservation() {
        let exclusions = Exclusions::new();
        let artifacts = ArtifactKey::new("library", "managed-game-artifacts");
        let first = exclusions
            .try_acquire_read_artifacts(["instance-a"], [artifacts.clone(), artifacts.clone()])
            .unwrap();
        assert_eq!(first.artifact_count(), 1);
        let receipt = first.clone();
        let last_receipt = receipt.clone();
        let second = exclusions
            .try_acquire_read_artifacts(["instance-b"], [artifacts.clone()])
            .unwrap();
        drop((first, receipt, second));
        assert!(matches!(
            exclusions.try_acquire(["instance-b"], [artifacts.clone()]),
            Err(ExclusionError::Busy)
        ));
        assert!(matches!(
            exclusions.try_acquire(["instance-a"], []),
            Err(ExclusionError::Busy)
        ));
        drop(last_receipt);
        let writer = exclusions
            .try_acquire(["instance-a", "instance-b"], [artifacts.clone()])
            .unwrap();
        let writer_receipt = writer.clone();
        drop(writer);
        assert!(matches!(
            exclusions.try_acquire_read_artifacts(["instance-c"], [artifacts.clone()]),
            Err(ExclusionError::Busy)
        ));
        drop(writer_receipt);
        assert!(
            exclusions
                .try_acquire_read_artifacts(["instance-c"], [artifacts])
                .is_ok()
        );
    }

    #[test]
    fn simultaneous_readers_and_writer_cannot_obtain_conflicting_authority() {
        const READERS: usize = 8;
        for _ in 0..16 {
            let exclusions = Exclusions::new();
            let artifacts = ArtifactKey::new("library", "managed-game-artifacts");
            let start = Arc::new(Barrier::new(READERS + 2));
            let readers = (0..READERS)
                .map(|index| {
                    let exclusions = exclusions.clone();
                    let artifacts = artifacts.clone();
                    let start = Arc::clone(&start);
                    thread::spawn(move || {
                        start.wait();
                        exclusions
                            .try_acquire_read_artifacts([format!("instance-{index}")], [artifacts])
                    })
                })
                .collect::<Vec<_>>();
            let writer = {
                let exclusions = exclusions.clone();
                let artifacts = artifacts.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    exclusions.try_acquire(["installer"], [artifacts])
                })
            };
            start.wait();
            // Join results retain successful leases, so none can disappear
            // before every contender has made its admission decision.
            let readers = readers
                .into_iter()
                .map(|reader| reader.join().unwrap())
                .collect::<Vec<_>>();
            let writer = writer.join().unwrap();
            if writer.is_ok() {
                assert!(
                    readers
                        .iter()
                        .all(|result| matches!(result, Err(ExclusionError::Busy)))
                );
            } else {
                assert!(matches!(writer, Err(ExclusionError::Busy)));
                assert!(readers.iter().all(Result::is_ok));
            }
            drop((writer, readers));
            assert!(exclusions.try_acquire(["installer"], [artifacts]).is_ok());
        }
    }
}
