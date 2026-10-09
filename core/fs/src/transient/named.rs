//! Portable transient storage backed by the existing exact named-stage owner.
//!
//! No caller path is introduced. Live cleanup and publication use StageToken's
//! native receipts; a process crash can leave an unclaimed scratch file. Such
//! leftovers are preserved, never discovered or deleted by their name prefix.

use super::*;
use crate::{
    FileCreateObligation, FileCreateOutcome, FileCreateResolution, FilePromotionObligation,
    FilePromotionOutcome, FilePromotionResolution, FileRevision, SealedStagedFile,
    StageDiscardObligation, StageDiscardOutcome, StageDiscardResolution, StagedFile,
};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::sync::Mutex;

pub(crate) struct TransientFile {
    pub(super) stage_id: u64,
    identity: platform::Identity,
    directory: Directory,
    destination: LeafName,
    storage: Mutex<Storage>,
}

struct Storage {
    state: Option<State>,
    digest: Option<[u8; 32]>,
    verified: bool,
    sealed_revision: Option<FileRevision>,
}

enum State {
    Writing {
        stage: StagedFile,
        hasher: Sha256,
        position: u64,
    },
    Sealed(SealedStagedFile),
    Promotion(Box<FilePromotionObligation>),
    Published {
        file: FileCapability,
        revision: Option<FileRevision>,
    },
    Discard(StageDiscardObligation),
    Discarded,
}

pub(super) enum DiscardTransientFileError {
    Retained {
        error: io::Error,
        file: TransientFile,
    },
}

pub(super) fn create_stage(destination: TransientDestination) -> TransientStageCreateOutcome {
    match destination.directory.create_stage() {
        FileCreateOutcome::Created(stage) => finish_creation(destination, stage),
        FileCreateOutcome::NoEffect(error) => {
            TransientStageCreateOutcome::NoEffect { error, destination }
        }
        FileCreateOutcome::AppliedUnverified(obligation) => {
            creation_pending(destination, obligation)
        }
    }
}

fn creation_pending(
    destination: TransientDestination,
    obligation: FileCreateObligation,
) -> TransientStageCreateOutcome {
    TransientStageCreateOutcome::Pending(TransientCreationObligation {
        error: io::Error::new(obligation.error().kind(), obligation.error().to_string()),
        state: Some(TransientCreationState::Named {
            destination,
            obligation,
        }),
    })
}

pub(super) fn reconcile_creation(
    destination: TransientDestination,
    obligation: FileCreateObligation,
) -> TransientStageCreateOutcome {
    match obligation.reconcile() {
        FileCreateResolution::Created(stage) => finish_creation(destination, stage),
        FileCreateResolution::NoEffect(error) => {
            TransientStageCreateOutcome::NoEffect { error, destination }
        }
        FileCreateResolution::Indeterminate(obligation) => {
            creation_pending(destination, obligation)
        }
    }
}

fn finish_creation(
    mut destination: TransientDestination,
    staged: StagedFile,
) -> TransientStageCreateOutcome {
    let identity = staged.file.identity;
    let stage_id = staged.token.id;
    let file = TransientFile {
        stage_id,
        identity,
        directory: destination.directory.clone(),
        destination: destination.name.clone(),
        storage: Mutex::new(Storage {
            state: Some(State::Writing {
                stage: staged,
                hasher: Sha256::new(),
                position: 0,
            }),
            digest: None,
            verified: false,
            sealed_revision: None,
        }),
    };
    let token = destination
        .token
        .take()
        .expect("destination retains reservation")
        .into_effect_token();
    let stage = TransientStage {
        destination: Some(destination),
        file: Some(file),
        identity,
        position: 0,
        token: Some(token),
    };
    match stage
        .token
        .as_ref()
        .expect("stage retains reservation")
        .mark_live(identity, stage_id)
    {
        Ok(()) => TransientStageCreateOutcome::Created(stage),
        Err(error) => TransientStageCreateOutcome::Pending(TransientCreationObligation {
            error,
            state: Some(TransientCreationState::Stage(stage)),
        }),
    }
}

fn lock(file: &TransientFile) -> io::Result<std::sync::MutexGuard<'_, Storage>> {
    file.storage
        .lock()
        .map_err(|_| io::Error::other("named transient storage lock was poisoned"))
}

fn exact_file(file: &FileCapability, expected: platform::Identity) -> io::Result<()> {
    file.revision()?;
    if platform::named_stage_evidence(&file.handle)? != (expected, 1) {
        return Err(crate::identity_changed(
            "named transient acquired a different identity or link",
        ));
    }
    Ok(())
}

pub(super) fn write_transient_at(
    file: &TransientFile,
    bytes: &[u8],
    offset: u64,
) -> io::Result<usize> {
    let mut storage = lock(file)?;
    let Some(State::Writing {
        stage,
        hasher,
        position,
    }) = storage.state.as_mut()
    else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    if *position != offset {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    exact_file(&stage.file, file.identity)?;
    // StagedFile::writer starts a new truncating writer. Transfer frames must
    // instead append through the already admitted handle and retained offset.
    let written = platform::write_at(&stage.file.handle, bytes, offset)?;
    hasher.update(&bytes[..written]);
    *position = position
        .checked_add(written as u64)
        .ok_or_else(|| io::Error::other("named transient size overflowed"))?;
    Ok(written)
}

pub(super) fn seal_transient_file(
    file: &mut TransientFile,
    expected: platform::Identity,
    size: u64,
) -> io::Result<()> {
    if file.identity != expected {
        return Err(stale_capability());
    }
    let mut storage = lock(file)?;
    if let Some(State::Writing { position, .. }) = storage.state.as_ref() {
        if *position != size {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let State::Writing { stage, hasher, .. } = storage.state.take().expect("writing state")
        else {
            unreachable!()
        };
        match stage.seal() {
            Ok(stage) => {
                storage.digest = Some(hasher.finalize().into());
                storage.state = Some(State::Sealed(stage));
            }
            Err(failure) => {
                let error = io::Error::new(failure.error().kind(), failure.error().to_string());
                storage.state = Some(State::Writing {
                    stage: failure.into_staged(),
                    hasher,
                    position: size,
                });
                return Err(error);
            }
        }
    }
    let Some(State::Sealed(stage)) = storage.state.as_ref() else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    exact_file(&stage.file, expected)?;
    stage.file.validate_revision(&stage.revision)?;
    if stage.revision.size != size {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut offset = 0;
    while offset < size {
        let allowed = (size - offset).min(buffer.len() as u64) as usize;
        let read = platform::read_at(&stage.file.handle, &mut buffer[..allowed], offset)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
    }
    stage.file.validate_revision(&stage.revision)?;
    exact_file(&stage.file, expected)?;
    let digest: [u8; 32] = hasher.finalize().into();
    if Some(digest) != storage.digest {
        return Err(crate::identity_changed(
            "named transient bytes differ from the admitted stream",
        ));
    }
    let revision = copy_revision(&stage.revision);
    storage.sealed_revision = Some(revision);
    storage.verified = true;
    Ok(())
}

fn copy_revision(revision: &FileRevision) -> FileRevision {
    FileRevision {
        authority: revision.authority.clone(),
        identity: revision.identity,
        size: revision.size,
        stamp: revision.stamp,
    }
}

fn validate_sealed(file: &TransientFile, storage: &Storage) -> io::Result<()> {
    let Some(State::Sealed(stage)) = storage.state.as_ref() else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    if !storage.verified {
        return Err(io::ErrorKind::InvalidData.into());
    }
    exact_file(&stage.file, file.identity)?;
    stage.file.validate_revision(&stage.revision)
}

pub(super) fn read_transient_at(
    file: &TransientFile,
    bytes: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let storage = lock(file)?;
    validate_sealed(file, &storage)?;
    let Some(State::Sealed(stage)) = storage.state.as_ref() else {
        unreachable!()
    };
    let read = platform::read_at(&stage.file.handle, bytes, offset)?;
    validate_sealed(file, &storage)?;
    Ok(read)
}

pub(super) fn validate_unpublished(
    file: &TransientFile,
    expected: platform::Identity,
) -> io::Result<()> {
    if file.identity != expected {
        return Err(stale_capability());
    }
    let storage = lock(file)?;
    let Some(State::Sealed(stage)) = storage.state.as_ref() else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    // Changed bytes forbid publication, but the exact scratch binding still
    // proves that this member is unpublished and can be discarded safely.
    exact_file(&stage.file, expected)
}

pub(super) fn link_transient_file(
    file: &mut TransientFile,
    parent: &platform::DirectoryHandle,
    name: &OsStr,
) -> io::Result<()> {
    if platform::directory_identity(parent)? != file.directory.inner.identity.physical
        || name != file.destination.as_os_str()
    {
        return Err(stale_capability());
    }
    let mut storage = lock(file)?;
    validate_sealed(file, &storage)?;
    let Some(State::Sealed(stage)) = storage.state.take() else {
        unreachable!()
    };
    match stage.promote_no_replace(&file.directory, &file.directory, &file.destination) {
        FilePromotionOutcome::Applied(published) => {
            storage.state = Some(State::Published {
                file: published,
                revision: None,
            });
            validate_published(file, &mut storage)
        }
        FilePromotionOutcome::NoEffect { error, staged } => {
            storage.state = Some(State::Sealed(staged));
            Err(error)
        }
        FilePromotionOutcome::AppliedUnverified(obligation) => {
            let error = io::Error::new(obligation.error().kind(), obligation.error().to_string());
            storage.state = Some(State::Promotion(obligation));
            Err(error)
        }
    }
}

fn reconcile_promotion(storage: &mut Storage) {
    if matches!(storage.state, Some(State::Promotion(_))) {
        let Some(State::Promotion(obligation)) = storage.state.take() else {
            unreachable!()
        };
        storage.state = Some(match obligation.reconcile() {
            FilePromotionResolution::Applied(file) => State::Published {
                file,
                revision: None,
            },
            FilePromotionResolution::NoEffect(stage) => State::Sealed(stage),
            FilePromotionResolution::Indeterminate(obligation) => State::Promotion(obligation),
        });
    }
}

fn validate_published(owner: &TransientFile, storage: &mut Storage) -> io::Result<()> {
    let expected = storage
        .sealed_revision
        .as_ref()
        .ok_or_else(stale_capability)?;
    let Some(State::Published { file, revision }) = storage.state.as_mut() else {
        return Err(io::ErrorKind::WouldBlock.into());
    };
    exact_file(file, owner.identity)?;
    let authority = file.parent.authority()?;
    let operation = authority.enter()?;
    file.validate_content_revision_in(&operation, expected)?;
    if revision.is_none() {
        *revision = Some(file.revision()?);
    }
    file.validate_revision_in(
        &operation,
        revision.as_ref().expect("published revision retained"),
    )?;
    file.validate_content_revision_in(&operation, expected)
}

pub(super) fn transient_publication_state(
    file: &TransientFile,
    parent: &platform::DirectoryHandle,
    name: &OsStr,
    expected: platform::Identity,
) -> io::Result<platform::TransientPublicationState> {
    if expected != file.identity
        || platform::directory_identity(parent)? != file.directory.inner.identity.physical
        || name != file.destination.as_os_str()
    {
        return Err(stale_capability());
    }
    let mut storage = lock(file)?;
    reconcile_promotion(&mut storage);
    match storage.state.as_ref() {
        Some(State::Published { .. }) => {
            validate_published(file, &mut storage)?;
            Ok(platform::TransientPublicationState::Published)
        }
        Some(State::Sealed(stage)) => {
            exact_file(&stage.file, expected)?;
            Ok(platform::TransientPublicationState::Unpublished)
        }
        Some(State::Writing { stage, .. }) => {
            exact_file(&stage.file, expected)?;
            Ok(platform::TransientPublicationState::Unpublished)
        }
        _ => Ok(platform::TransientPublicationState::Indeterminate),
    }
}

pub(super) fn transient_file_evidence(
    file: &TransientFile,
) -> io::Result<(platform::Identity, u64)> {
    let mut storage = lock(file)?;
    validate_published(file, &mut storage)?;
    let Some(State::Published { file, .. }) = storage.state.as_ref() else {
        unreachable!()
    };
    platform::named_stage_evidence(&file.handle)
}

fn discard(file: &TransientFile, expected: platform::Identity) -> io::Result<()> {
    if file.identity != expected {
        return Err(stale_capability());
    }
    let mut storage = lock(file)?;
    reconcile_promotion(&mut storage);
    match storage.state.as_ref() {
        // Native cleanup must also see missing scratch bindings: its retained
        // receipt can prove an already-unlinked file has no remaining effect.
        // It still refuses a replacement at the original scratch name.
        Some(State::Writing { .. } | State::Sealed(_) | State::Discard(_)) => {}
        Some(State::Discarded) => return Ok(()),
        _ => return Err(io::ErrorKind::WouldBlock.into()),
    }
    let outcome = match storage.state.take().expect("discard retains state") {
        State::Writing { stage, .. } => stage.discard(),
        State::Sealed(stage) => stage.discard(),
        State::Discard(obligation) => match obligation.reconcile() {
            StageDiscardResolution::Discarded => StageDiscardOutcome::Discarded,
            StageDiscardResolution::Indeterminate(obligation) => {
                StageDiscardOutcome::AppliedUnverified(obligation)
            }
        },
        _ => unreachable!(),
    };
    match outcome {
        StageDiscardOutcome::Discarded => {
            storage.state = Some(State::Discarded);
            Ok(())
        }
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            let error = io::Error::new(obligation.error().kind(), obligation.error().to_string());
            storage.state = Some(State::Discard(obligation));
            Err(error)
        }
    }
}

pub(super) fn discard_transient_file(
    file: TransientFile,
    expected: platform::Identity,
) -> Result<(), DiscardTransientFileError> {
    match discard(&file, expected) {
        Ok(()) => Ok(()),
        Err(error) => Err(DiscardTransientFileError::Retained { error, file }),
    }
}

pub(super) fn into_published_file(file: TransientFile) -> File {
    let storage = file
        .storage
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(State::Published { file, .. }) = storage.state else {
        panic!("published transition retains published file")
    };
    file.handle
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{RootRevokeOutcome, RootSession, RootSessionAcquireOutcome};
    use std::path::{Path, PathBuf};

    fn fixture() -> (tempfile::TempDir, RootSession, Directory) {
        let temp =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let session = acquire(temp.path());
        let root = session.root().unwrap();
        (temp, session, root)
    }

    fn acquire(path: &Path) -> RootSession {
        match RootSession::acquire(path) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            other => panic!("strict named root admission failed: {other:?}"),
        }
    }

    fn stage(root: &Directory, name: &str) -> TransientStage {
        let destination = root
            .admit_transient_destination(LeafName::new(name).unwrap())
            .unwrap();
        match destination.create_stage() {
            TransientStageCreateOutcome::Created(stage) => stage,
            other => panic!("named staging must work on this host: {other:?}"),
        }
    }

    fn scratch(temp: &Path, stage: &TransientStage) -> PathBuf {
        let storage = lock(stage.file.as_ref().unwrap()).unwrap();
        let name = match storage.state.as_ref().unwrap() {
            State::Writing { stage, .. } => &stage.file.name,
            State::Sealed(stage) => &stage.file.name,
            _ => panic!("expected unpublished stage"),
        };
        temp.join(name.as_os_str())
    }

    fn cancel(outcome: TransientDiscardOutcome) {
        let TransientDiscardOutcome::Discarded(destination) = outcome else {
            panic!("discard must settle")
        };
        assert!(matches!(
            destination.cancel(),
            TransientDestinationCancelOutcome::Cancelled
        ));
    }

    fn finish(session: RootSession, root: Directory) {
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn named_streams_multiple_frames_reads_seeks_and_publishes_same_file() {
        let (temp, session, root) = fixture();
        let mut stage = stage(&root, "artifact.bin");
        let identity = stage.identity;
        let scratch = scratch(temp.path(), &stage);
        let chunk = [0x5au8; 64 * 1024];
        for _ in 0..272 {
            stage.write_all(&chunk).unwrap();
        }
        stage.write_all(b"tail").unwrap();
        let mut sealed = stage.seal().unwrap();
        assert_eq!(sealed.size(), 17 * 1024 * 1024 + 4);
        sealed.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = [0; 4];
        sealed.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"tail");
        let batch = TransientPublicationBatch::new(vec![sealed]).unwrap();
        let TransientPublicationBatchOutcome::Published(files) = batch.publish_create_new() else {
            panic!("named publication must settle")
        };
        assert!(files[0].identity == identity);
        assert!(platform::named_stage_evidence(&files[0].handle).unwrap() == (identity, 1));
        assert!(!scratch.exists());
        assert_eq!(
            std::fs::metadata(temp.path().join("artifact.bin"))
                .unwrap()
                .len(),
            17 * 1024 * 1024 + 4
        );
        drop(files);
        finish(session, root);
    }

    #[test]
    fn named_readback_rejects_changed_bytes_before_sealing() {
        let (temp, session, root) = fixture();
        let mut stage = stage(&root, "artifact.bin");
        stage.write_all(b"original").unwrap();
        std::fs::write(scratch(temp.path(), &stage), b"tampered").unwrap();
        let failure = stage
            .seal()
            .expect_err("stream digest must match staged bytes");
        assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
        cancel(failure.into_stage().discard());
        assert!(!temp.path().join("artifact.bin").exists());
        finish(session, root);
    }

    #[test]
    fn named_revision_fences_reads_and_publication_after_sealing() {
        let (temp, session, root) = fixture();
        let mut stage = stage(&root, "artifact.bin");
        stage.write_all(b"original").unwrap();
        let scratch = scratch(temp.path(), &stage);
        let mut sealed = stage.seal().unwrap();
        std::fs::write(&scratch, b"tampered").unwrap();
        assert!(sealed.read(&mut [0; 8]).is_err());
        let result = TransientPublicationBatch::new(vec![sealed])
            .unwrap()
            .publish_create_new();
        let TransientPublicationBatchOutcome::NoEffect { error, batch } = result else {
            panic!("changed bytes must prevent publication without blocking discard")
        };
        let obligation = TransientPublicationBatchObligation {
            error,
            batch: Some(batch),
        };
        let TransientPublicationBatchOutcome::NoEffect { batch, .. } = obligation.reconcile()
        else {
            panic!("changed unpublished bytes must also reconcile for discard")
        };
        for stage in batch.into_stages() {
            cancel(stage.discard());
        }
        finish(session, root);
        assert!(!scratch.exists());
        assert!(!temp.path().join("artifact.bin").exists());
    }

    #[test]
    fn named_replaced_sealed_scratch_remains_unresolved() {
        let (temp, session, root) = fixture();
        let mut stage = stage(&root, "artifact.bin");
        stage.write_all(b"owned").unwrap();
        let scratch = scratch(temp.path(), &stage);
        let sealed = stage.seal().unwrap();
        let moved = temp.path().join("moved-own-stage");
        std::fs::rename(&scratch, &moved).unwrap();
        std::fs::write(&scratch, b"external").unwrap();
        let TransientPublicationBatchOutcome::Pending(obligation) =
            TransientPublicationBatch::new(vec![sealed])
                .unwrap()
                .publish_create_new()
        else {
            panic!("replacement scratch must retain publication recovery")
        };
        let TransientPublicationBatchOutcome::Pending(obligation) = obligation.reconcile() else {
            panic!("replacement identity must remain unresolved")
        };
        assert_eq!(std::fs::read(&scratch).unwrap(), b"external");
        assert!(!temp.path().join("artifact.bin").exists());
        std::fs::remove_file(&scratch).unwrap();
        std::fs::rename(&moved, &scratch).unwrap();
        let TransientPublicationBatchOutcome::NoEffect { batch, .. } = obligation.reconcile()
        else {
            panic!("restored scratch identity must permit discard despite revision drift")
        };
        for stage in batch.into_stages() {
            cancel(stage.discard());
        }
        finish(session, root);
        assert!(!scratch.exists());
        assert!(!temp.path().join("artifact.bin").exists());
    }

    #[test]
    fn named_partial_batch_retains_order_and_preserves_collision() {
        let (temp, session, root) = fixture();
        let mut first = stage(&root, "first.bin");
        let mut second = stage(&root, "second.bin");
        first.write_all(b"first").unwrap();
        second.write_all(b"second").unwrap();
        let mut first = first.seal().unwrap();
        let second = second.seal().unwrap();
        link_transient_file(
            first.stage.file.as_mut().unwrap(),
            root.inner.handle(),
            OsStr::new("first.bin"),
        )
        .unwrap();
        std::fs::write(temp.path().join("second.bin"), b"external").unwrap();
        let obligation = TransientPublicationBatchObligation {
            error: io::ErrorKind::AlreadyExists.into(),
            batch: Some(TransientPublicationBatch::new(vec![first, second]).unwrap()),
        };
        let TransientPublicationBatchOutcome::Partial { mut members, .. } = obligation.reconcile()
        else {
            panic!("partial batch must classify both members")
        };
        let TransientPublicationMember::Unpublished(second) = members.pop().unwrap() else {
            panic!("second remains unpublished")
        };
        cancel(second.discard());
        assert!(matches!(
            members.pop(),
            Some(TransientPublicationMember::Published(_))
        ));
        assert_eq!(
            std::fs::read(temp.path().join("first.bin")).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(temp.path().join("second.bin")).unwrap(),
            b"external"
        );
        finish(session, root);
    }

    #[test]
    fn named_dropped_cleanup_stays_fenced_until_exact_stage_returns() {
        let (temp, session, root) = fixture();
        let mut stage = stage(&root, "artifact.bin");
        stage.write_all(b"owned").unwrap();
        let original = scratch(temp.path(), &stage);
        let moved = temp.path().join("moved-own-stage");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::write(&original, b"external").unwrap();
        let pending = stage.discard();
        assert!(matches!(pending, TransientDiscardOutcome::Pending(_)));
        drop(pending);
        drop(root);
        let RootRevokeOutcome::Pending(drain) = session.revoke() else {
            panic!("replacement must retain root fence")
        };
        assert_eq!(std::fs::read(&original).unwrap(), b"external");
        std::fs::remove_file(&original).unwrap();
        std::fs::rename(&moved, &original).unwrap();
        assert!(matches!(drain.try_settle(), RootRevokeOutcome::Revoked));
        assert!(!original.exists());
    }

    #[test]
    fn named_deleted_scratch_explicit_discard_settles() {
        for seal in [false, true] {
            let (temp, session, root) = fixture();
            let mut stage = stage(&root, "artifact.bin");
            stage.write_all(b"owned").unwrap();
            let scratch = scratch(temp.path(), &stage);
            let outcome = if seal {
                let stage = stage.seal().unwrap();
                std::fs::remove_file(&scratch).unwrap();
                stage.discard()
            } else {
                std::fs::remove_file(&scratch).unwrap();
                stage.discard()
            };
            let TransientDiscardOutcome::Pending(obligation) = outcome else {
                panic!("missing scratch must retain native cleanup until reconciled")
            };
            cancel(obligation.reconcile());
            assert!(!scratch.exists());
            assert!(!temp.path().join("artifact.bin").exists());
            finish(session, root);
        }
    }

    #[test]
    fn named_deleted_scratch_drop_drains() {
        for seal in [false, true] {
            let (temp, session, root) = fixture();
            let mut stage = stage(&root, "artifact.bin");
            stage.write_all(b"owned").unwrap();
            let scratch = scratch(temp.path(), &stage);
            if seal {
                let stage = stage.seal().unwrap();
                std::fs::remove_file(&scratch).unwrap();
                drop(stage);
            } else {
                std::fs::remove_file(&scratch).unwrap();
                drop(stage);
            }
            finish(session, root);
            assert!(!scratch.exists());
            assert!(!temp.path().join("artifact.bin").exists());
        }
    }

    #[test]
    fn named_513_members_publish_in_bounded_windows_without_cap_increase() {
        let (temp, session, root) = fixture();
        let names = (0..513)
            .map(|index| LeafName::new(format!("member-{index}.bin")).unwrap())
            .collect::<Vec<_>>();
        assert!(
            root.admit_transient_destinations(names[..257].to_vec())
                .is_err()
        );
        for names in names.chunks(MAX_TRANSIENT_BATCH) {
            let batch = root.admit_transient_destinations(names.to_vec()).unwrap();
            let mut stages = Vec::new();
            for destination in batch.into_destinations() {
                let TransientStageCreateOutcome::Created(mut stage) = destination.create_stage()
                else {
                    panic!("bounded named stage must be created")
                };
                stage.write_all(b"payload").unwrap();
                stages.push(stage.seal().unwrap());
            }
            let outcome = TransientPublicationBatch::new(stages)
                .unwrap()
                .publish_create_new();
            assert!(matches!(
                outcome,
                TransientPublicationBatchOutcome::Published(_)
            ));
            drop(outcome);
        }
        assert_eq!(
            std::fs::read(temp.path().join("member-512.bin")).unwrap(),
            b"payload"
        );
        finish(session, root);
    }

    #[test]
    fn named_process_exit_preserves_unclaimed_scratch_without_adopting_prefixes() {
        const CHILD_ROOT: &str = "AXIAL_NAMED_STAGE_CRASH_TEST_ROOT";
        if let Some(path) = std::env::var_os(CHILD_ROOT) {
            let session = acquire(Path::new(&path));
            let root = session.root().unwrap();
            let mut stage = stage(&root, "never-published.bin");
            stage.write_all(b"interrupted transfer").unwrap();
            std::process::exit(0); // No Rust destructors, like a terminated producer.
        }
        let temp =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        std::fs::write(temp.path().join(".axial-stage-user-file"), b"user").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "transient::named::tests::named_process_exit_preserves_unclaimed_scratch_without_adopting_prefixes", "--nocapture"])
            .env(CHILD_ROOT, temp.path()).status().unwrap();
        assert!(status.success());
        let before = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            before
                .iter()
                .filter(|name| name.to_string_lossy().starts_with(".axial-stage-"))
                .count(),
            2
        );
        let session = acquire(temp.path());
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        let after = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(before, after);
        assert!(!temp.path().join("never-published.bin").exists());
    }
}
