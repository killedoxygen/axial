//! A durable interrupted-reset fence, not cross-process deletion authority.
//! Only a newly confirmed caller may construct a reset from a reopened session.
//! The normal profile marker remains present through every deletion boundary.

use super::*;
use sha2::{Digest, Sha256};

const INTENT_NAME: &str = ".axial-reset-intent";
// v1 writers did not fence unknown launch children after restart. Never adopt
// those records: matching filesystem bindings cannot prove child settlement.
const MAGIC: &[u8] = b"axial-root-reset-v2\0";
const ANCHOR_LIMIT: u64 = 4096;

enum Publication {
    Creating(FileCreateObligation),
    Writing(StagedFile),
    Sealing(StagedFile),
    Publishing(SealedStagedFile),
    Reconciling(Box<FilePromotionObligation>),
}

/// Retains every effect and the exact native root across reset retries. The
/// caller must first drain application services and explicitly accept deleting
/// the current tree. It must also prove no application launch child is unknown;
/// draining this process's tasks alone cannot establish that after restart.
/// An interrupted-reset probe alone is never acceptance of current-tree deletion.
#[must_use = "retain the accepted reset until try_clear succeeds"]
pub struct PendingRootReset {
    session: Option<RootSession>,
    anchor: LeafName,
    expected_anchor: Vec<u8>,
    marker: Option<FileCapability>,
    intent: Option<FileCapability>,
    publication: Option<Publication>,
    reset: Option<ResetStartOutcome>,
    cleared: bool,
    complete: bool,
}

impl fmt::Debug for PendingRootReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingRootReset").finish_non_exhaustive()
    }
}

impl RootSession {
    /// Inspect only after normal lease/recovery admission. A valid fence means
    /// startup must stop before opening services and ask for fresh confirmation.
    /// Malformed or replaced evidence is an error, never deletion permission.
    pub fn interrupted_reset(&self, anchor: &LeafName) -> io::Result<bool> {
        let root = self.root()?;
        let intent = match root.open_file(&intent_name()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let marker = root.open_file(anchor)?;
        let operation = self.authority.enter()?;
        let marker_bytes = read_anchor(&marker, &operation)?;
        let expected = intent_bytes(&self.authority, &marker, &marker_bytes, intent.identity)?;
        if read_anchor(&intent, &operation)? != expected {
            return Err(identity_changed("interrupted reset evidence changed"));
        }
        Ok(true)
    }
}

impl PendingRootReset {
    pub fn new(session: RootSession, anchor: LeafName, expected_anchor: Vec<u8>) -> Self {
        Self {
            session: Some(session),
            anchor,
            expected_anchor,
            marker: None,
            intent: None,
            publication: None,
            reset: None,
            cleared: false,
            complete: false,
        }
    }

    /// Durably record the already accepted reset before deleting anything.
    /// A sealed create-only stage is sufficient to fence the next startup:
    /// ordinary root replay finishes its exact publication before this probe.
    pub fn prepare(&mut self) -> io::Result<()> {
        if self.complete || self.reset.is_some() {
            return Ok(());
        }
        if self.expected_anchor.is_empty()
            || self.expected_anchor.len() > ANCHOR_LIMIT as usize
            || leaf_names_equivalent(self.anchor.as_os_str(), OsStr::new(INTENT_NAME))
            || leaf_names_equivalent(self.anchor.as_os_str(), OsStr::new(ROOT_LEASE_NAME))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reset anchor is invalid",
            ));
        }
        let session = self.session.as_ref().expect("preparation retains session");
        let root = session.root()?;
        if self.marker.is_none() {
            self.marker = Some(root.open_file(&self.anchor)?);
        }
        let marker = self.marker.as_ref().expect("preparation retains marker");
        {
            let operation = session.authority.enter()?;
            if read_anchor(marker, &operation)? != self.expected_anchor {
                return Err(identity_changed("reset profile marker changed"));
            }
        }
        for _ in 0..8 {
            if let Some(intent) = &self.intent {
                let operation = session.authority.enter()?;
                if read_anchor(intent, &operation)?
                    != intent_bytes(
                        &session.authority,
                        marker,
                        &self.expected_anchor,
                        intent.identity,
                    )?
                {
                    return Err(identity_changed("reset intent changed"));
                }
                return Ok(());
            }
            self.publication = match self.publication.take() {
                None => match root.open_file(&intent_name()) {
                    Ok(intent) => {
                        self.intent = Some(intent);
                        None
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        match root.create_recoverable_stage(&intent_name()) {
                            FileCreateOutcome::Created(stage) => Some(Publication::Writing(stage)),
                            FileCreateOutcome::NoEffect(error) => return Err(error),
                            FileCreateOutcome::AppliedUnverified(effect) => {
                                Some(Publication::Creating(effect))
                            }
                        }
                    }
                    Err(error) => return Err(error),
                },
                Some(Publication::Creating(effect)) => match effect.reconcile() {
                    FileCreateResolution::Created(stage) => Some(Publication::Writing(stage)),
                    FileCreateResolution::NoEffect(error) => return Err(error),
                    FileCreateResolution::Indeterminate(effect) => {
                        Some(Publication::Creating(effect))
                    }
                },
                Some(Publication::Writing(mut stage)) => {
                    let bytes = match intent_bytes(
                        &session.authority,
                        marker,
                        &self.expected_anchor,
                        stage.file.identity,
                    ) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            self.publication = Some(Publication::Writing(stage));
                            return Err(error);
                        }
                    };
                    if let Err(error) = stage.write_all(&bytes) {
                        self.publication = Some(Publication::Writing(stage));
                        return Err(error);
                    }
                    Some(Publication::Sealing(stage))
                }
                Some(Publication::Sealing(stage)) => match stage.seal() {
                    Ok(stage) => Some(Publication::Publishing(stage)),
                    Err(failure) => Some(Publication::Sealing(failure.into_staged())),
                },
                Some(Publication::Publishing(stage)) => {
                    match stage.promote_no_replace(&root, &root, &intent_name()) {
                        FilePromotionOutcome::Applied(intent) => {
                            self.intent = Some(intent);
                            None
                        }
                        FilePromotionOutcome::NoEffect { staged, .. } => {
                            Some(Publication::Publishing(staged))
                        }
                        FilePromotionOutcome::AppliedUnverified(effect) => {
                            Some(Publication::Reconciling(effect))
                        }
                    }
                }
                Some(Publication::Reconciling(effect)) => match effect.reconcile() {
                    FilePromotionResolution::Applied(intent) => {
                        self.intent = Some(intent);
                        None
                    }
                    FilePromotionResolution::NoEffect(stage) => {
                        Some(Publication::Publishing(stage))
                    }
                    FilePromotionResolution::Indeterminate(effect) => {
                        Some(Publication::Reconciling(effect))
                    }
                },
            };
        }
        Err(io::Error::other(
            "reset intent publication remains unsettled",
        ))
    }

    pub fn try_clear(&mut self) -> io::Result<()> {
        if self.complete {
            return Ok(());
        }
        self.prepare()?;
        if self.reset.is_none() {
            self.reset = Some(
                self.session
                    .take()
                    .expect("prepared reset retains session")
                    .begin_reset(),
            );
        }
        for _ in 0..8 {
            let outcome = self.reset.take().expect("reset outcome remains retained");
            self.reset = Some(match outcome {
                ResetStartOutcome::Ready(authority) => {
                    let result = self.clear(&authority);
                    if let Err(error) = result {
                        self.reset = Some(ResetStartOutcome::Ready(authority));
                        return Err(error);
                    }
                    match authority.release() {
                        Ok(()) => {
                            self.marker.take();
                            self.intent.take();
                            self.complete = true;
                            return Ok(());
                        }
                        Err(authority) => ResetStartOutcome::Ready(authority),
                    }
                }
                ResetStartOutcome::Pending(drain) => drain.try_settle(),
                ResetStartOutcome::Refused(failure) => failure.retry(),
                ResetStartOutcome::Failed(failure) => failure.retry(),
                ResetStartOutcome::Recovery { recovery } => {
                    if recovery.file_count() > 0 || recovery.directory_count() > 0 {
                        recovery.remove_all()
                    } else {
                        match recovery.acknowledge_external() {
                            ResetStartOutcome::Recovery { recovery } => {
                                recovery.defer_managed_reset()
                            }
                            outcome => outcome,
                        }
                    }
                }
            });
        }
        Err(io::Error::other("reset authority remains unsettled"))
    }

    fn clear(&mut self, reset: &RootResetAuthority) -> io::Result<()> {
        let session = reset.session.as_ref().expect("reset retains session");
        let marker = self.marker.as_ref().expect("reset retains marker");
        let intent = self.intent.as_ref().expect("reset retains intent");
        {
            let operation = session.authority.enter_reset_operation()?;
            if read_anchor(marker, &operation)? != self.expected_anchor {
                return Err(identity_changed("reset marker changed before clear"));
            }
            if !self.cleared
                && read_anchor(intent, &operation)?
                    != intent_bytes(
                        &session.authority,
                        marker,
                        &self.expected_anchor,
                        intent.identity,
                    )?
            {
                return Err(identity_changed("reset intent changed before clear"));
            }
        }
        if !self.cleared {
            reset.try_clear_root(&[
                (marker.name.as_os_str(), marker.identity),
                (intent.name.as_os_str(), intent.identity),
            ])?;
            self.cleared = true;
        }
        // Once cleared, retries never traverse/delete the tree again. The final
        // exact intent unlink follows durable proof that only anchors remain.
        let operation = session.authority.enter_reset_operation()?;
        if read_anchor(marker, &operation)? != self.expected_anchor {
            return Err(identity_changed("reset marker changed before completion"));
        }
        // A failed final unlink may leave the same file in place. Identity is
        // not a content proof: revalidate it even after the tree has cleared.
        // Only absence is delegated to the native retained-unlinked proof.
        if platform::file_binding_state(
            &intent.parent.inner.handle,
            intent.name.as_os_str(),
            intent.identity,
        )? != platform::BindingState::Absent
            && read_anchor(intent, &operation)?
                != intent_bytes(
                    &session.authority,
                    marker,
                    &self.expected_anchor,
                    intent.identity,
                )?
        {
            return Err(identity_changed("reset intent changed before completion"));
        }
        platform::finish_root_reset(
            &session.authority.root,
            &session.authority.lease,
            (marker.name.as_os_str(), marker.identity),
            (intent.name.as_os_str(), intent.identity, &intent.handle),
        )
    }
}

impl Drop for PendingRootReset {
    fn drop(&mut self) {
        // Prepared/live effects must never be silently converted into an
        // ordinary close. The owning native retry loop keeps this value alive.
        if !self.complete {
            std::process::abort();
        }
    }
}

fn intent_name() -> LeafName {
    LeafName::new(INTENT_NAME).expect("fixed reset intent name")
}

fn intent_bytes(
    authority: &CapabilityAuthority,
    marker: &FileCapability,
    bytes: &[u8],
    intent: platform::Identity,
) -> io::Result<Vec<u8>> {
    platform::validate_root(&authority.root)?;
    platform::validate_lease(&authority.lease)?;
    let lane = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem operation lock was poisoned"))?
        .recovery
        .lane_nonce;
    let mut encoded = MAGIC.to_vec();
    encoded.extend_from_slice(&platform::identity_witness(
        marker.parent.inner.identity.physical,
    ));
    encoded.extend_from_slice(&platform::lease_identity_witness(&authority.lease));
    encoded.extend_from_slice(&lane);
    encoded.extend_from_slice(&platform::identity_witness(marker.identity));
    encoded.extend_from_slice(&Sha256::digest(bytes));
    encoded.extend_from_slice(&platform::identity_witness(intent));
    encoded.extend_from_slice(&Sha256::digest(marker.name.as_os_str().as_encoded_bytes()));
    Ok(encoded)
}

fn read_anchor(file: &FileCapability, operation: &CapabilityOperation) -> io::Result<Vec<u8>> {
    file.validate(operation)?;
    let before = platform::file_receipt_fields(&file.handle)?;
    if before.0 > ANCHOR_LIMIT {
        return Err(identity_changed("reset anchor exceeds bound"));
    }
    let mut bytes = vec![0; before.0 as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        let count = platform::read_at(&file.handle, &mut bytes[offset..], offset as u64)?;
        if count == 0 {
            return Err(identity_changed("reset anchor shortened"));
        }
        offset += count;
    }
    if platform::file_receipt_fields(&file.handle)? != before {
        return Err(identity_changed("reset anchor changed while reading"));
    }
    file.validate(operation)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "profile-marker";
    const MARKER_BYTES: &[u8] = b"accepted isolated profile";

    fn fixture() -> (tempfile::TempDir, PendingRootReset, Directory) {
        let temporary = test_tempdir().unwrap();
        std::fs::write(temporary.path().join(MARKER), MARKER_BYTES).unwrap();
        std::fs::write(temporary.path().join("payload"), b"accepted payload").unwrap();
        let session = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("fixture root: {outcome:?}"),
        };
        assert!(
            !session
                .interrupted_reset(&LeafName::new(MARKER).unwrap())
                .unwrap()
        );
        let stale = session.root().unwrap();
        (
            temporary,
            PendingRootReset::new(
                session,
                LeafName::new(MARKER).unwrap(),
                MARKER_BYTES.to_vec(),
            ),
            stale,
        )
    }

    fn pause_after_clear(pending: &mut PendingRootReset) {
        pending.prepare().unwrap();
        let reset = match pending.session.take().unwrap().begin_reset() {
            ResetStartOutcome::Ready(reset) => reset,
            outcome => panic!("fixture reset: {outcome:?}"),
        };
        let marker = pending.marker.as_ref().unwrap();
        let intent = pending.intent.as_ref().unwrap();
        reset
            .try_clear_root(&[
                (marker.name.as_os_str(), marker.identity),
                (intent.name.as_os_str(), intent.identity),
            ])
            .unwrap();
        pending.cleared = true;
        pending.reset = Some(ResetStartOutcome::Ready(reset));
    }

    #[test]
    fn durable_fence_precedes_deletion_and_marker_and_old_capability_state_survive_correctly() {
        let (temporary, mut pending, stale) = fixture();
        pending.prepare().unwrap();
        assert!(
            pending
                .session
                .as_ref()
                .unwrap()
                .interrupted_reset(&LeafName::new(MARKER).unwrap())
                .unwrap()
        );
        assert!(temporary.path().join("payload").exists());
        assert_eq!(
            std::fs::read(temporary.path().join(MARKER)).unwrap(),
            MARKER_BYTES
        );
        pending.try_clear().unwrap();
        assert!(stale.open_file(&LeafName::new(MARKER).unwrap()).is_err());
        assert!(!temporary.path().join("payload").exists());
        assert!(!temporary.path().join(INTENT_NAME).exists());
        assert_eq!(
            std::fs::read(temporary.path().join(MARKER)).unwrap(),
            MARKER_BYTES
        );
        let session = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("reopen root: {outcome:?}"),
        };
        assert!(
            !session
                .interrupted_reset(&LeafName::new(MARKER).unwrap())
                .unwrap()
        );
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn v1_evidence_cannot_authorize_startup_reset_or_current_tree_deletion() {
        let (temporary, mut pending, _) = fixture();
        pending.prepare().unwrap();
        let path = temporary.path().join(INTENT_NAME);
        let current = std::fs::read(&path).unwrap();
        let current_magic = b"axial-root-reset-v2\0";
        let legacy_magic = b"axial-root-reset-v1\0";
        assert!(current.starts_with(current_magic));
        let mut legacy = current.clone();
        legacy[..legacy_magic.len()].copy_from_slice(legacy_magic);
        // Keep the exact inode and every root/lease/marker binding. The writer
        // version alone must prevent adopting this older acceptance record.
        std::fs::write(&path, &legacy).unwrap();
        assert!(
            pending
                .session
                .as_ref()
                .unwrap()
                .interrupted_reset(&LeafName::new(MARKER).unwrap())
                .is_err()
        );
        assert!(pending.prepare().is_err());
        assert!(pending.try_clear().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), legacy);
        assert_eq!(
            std::fs::read(temporary.path().join(MARKER)).unwrap(),
            MARKER_BYTES
        );
        assert_eq!(
            std::fs::read(temporary.path().join("payload")).unwrap(),
            b"accepted payload"
        );
        // Restore this test's original evidence only to settle its retained
        // owner; production never upgrades an interrupted v1 reset record.
        std::fs::write(&path, current).unwrap();
        pending.try_clear().unwrap();
    }

    #[test]
    fn changed_intent_after_clear_is_preserved_and_restored_bytes_allow_retry() {
        let (temporary, mut pending, _) = fixture();
        pause_after_clear(&mut pending);
        let path = temporary.path().join(INTENT_NAME);
        let original = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"changed same-inode intent").unwrap();
        assert!(pending.try_clear().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed same-inode intent");
        std::fs::write(&path, original).unwrap();
        pending.try_clear().unwrap();
    }

    #[test]
    fn finalization_never_reclears_new_entries_even_after_exact_intent_unlink() {
        let (temporary, mut pending, _) = fixture();
        pause_after_clear(&mut pending);
        let added = temporary.path().join("new-file");
        std::fs::write(&added, b"post-clear data").unwrap();
        assert!(pending.try_clear().is_err());
        assert!(temporary.path().join(INTENT_NAME).exists());
        // Simulate a completed exact unlink whose directory barrier was lost.
        std::fs::remove_file(temporary.path().join(INTENT_NAME)).unwrap();
        assert!(pending.try_clear().is_err());
        assert_eq!(std::fs::read(&added).unwrap(), b"post-clear data");
        let preserved = test_tempdir().unwrap();
        std::fs::rename(added, preserved.path().join("new-file")).unwrap();
        pending.try_clear().unwrap();
    }

    #[test]
    fn marker_and_intent_replacements_block_before_payload_deletion() {
        for name in [MARKER, INTENT_NAME] {
            let (temporary, mut pending, _) = fixture();
            pending.prepare().unwrap();
            let saved = test_tempdir().unwrap();
            let original = saved.path().join("original");
            let path = temporary.path().join(name);
            std::fs::rename(&path, &original).unwrap();
            std::fs::write(&path, b"replacement must survive").unwrap();
            assert!(pending.try_clear().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement must survive");
            assert!(temporary.path().join("payload").exists());
            std::fs::rename(&path, saved.path().join("replacement")).unwrap();
            std::fs::rename(original, path).unwrap();
            pending.try_clear().unwrap();
        }
    }

    #[test]
    fn replaced_root_and_lease_do_not_authorize_current_contents() {
        let (temporary, mut pending, _) = fixture();
        pending.prepare().unwrap();
        let saved = test_tempdir().unwrap();
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        std::fs::rename(&lease, saved.path().join("original-lease")).unwrap();
        std::fs::write(&lease, b"replacement lease").unwrap();
        assert!(pending.try_clear().is_err());
        assert!(temporary.path().join("payload").exists());
        std::fs::rename(&lease, saved.path().join("replacement-lease")).unwrap();
        std::fs::rename(saved.path().join("original-lease"), &lease).unwrap();
        let original_root = temporary.path().to_owned();
        let moved_root = saved.path().join("original-root");
        std::fs::rename(&original_root, &moved_root).unwrap();
        std::fs::create_dir(&original_root).unwrap();
        std::fs::write(original_root.join("replacement"), b"unrelated data").unwrap();
        assert!(pending.try_clear().is_err());
        assert!(moved_root.join("payload").exists());
        std::fs::rename(&original_root, saved.path().join("replacement-root")).unwrap();
        std::fs::rename(moved_root, &original_root).unwrap();
        pending.try_clear().unwrap();
    }

    #[test]
    fn case_alias_of_preserved_marker_is_not_deleted() {
        let (temporary, mut pending, _) = fixture();
        pending.prepare().unwrap();
        let alternate = temporary.path().join("PROFILE-MARKER");
        std::fs::rename(temporary.path().join(MARKER), &alternate).unwrap();
        if temporary.path().join(MARKER).exists() {
            // Exercise actual case-insensitive lookup, not a simulated equality.
            pending.try_clear().unwrap();
            assert_eq!(std::fs::read(alternate).unwrap(), MARKER_BYTES);
        } else {
            // On case-sensitive filesystems the moved binding must be refused.
            assert!(pending.try_clear().is_err());
            std::fs::rename(alternate, temporary.path().join(MARKER)).unwrap();
            pending.try_clear().unwrap();
        }
    }
}
