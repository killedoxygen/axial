//! Verified immutable FML inputs become create-only, process-writable instance files.

use super::coordinator::LaunchError;
use crate::{files::ScopedDirectory, install::artifacts::GameLibraries, tasks::CancellationToken};
use axial_fs::{
    DirectoryCreateObligation, DirectoryCreateOutcome, DirectoryCreatePreservation,
    DirectoryCreateResolution, DirectoryListingState, EntryKind, FileCapability, FileRevision,
    LeafName, leaf_names_equivalent,
};
use axial_minecraft::{
    download::{
        CreateOnlyTransferTarget, ExpectedTransferDigests, ManagedTransferAuthority,
        TransferCancellation, TransferCleanupObligation, TransferCleanupResolution,
        TransferContract, TransferFailureKind, TransferOutcome, TransferPublicationObligation,
        TransferPublicationOutcome, TransferUnsettledObligation, VerifiedTransferDiscardObligation,
        VerifiedTransferDiscardOutcome, transfer_cancellation_channel,
    },
    managed_path::ManagedLibraryFile,
};
use axial_resource::{PhysicalIoClass, PhysicalWorkRequest, process_physical_work};
use sha1::{Digest, Sha1};
use std::{
    io::{self, Read},
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

const MAX_DIRECTORY_ENTRIES: usize = 16_384;
// Complete bounded namespace listings dominate the streamed copy buffers.
const SCRATCH_BYTES: u64 = 64 << 20;

pub(super) struct Prepared {
    directory: ScopedDirectory,
    sources: Vec<ManagedLibraryFile>,
    files: Vec<FileProof>,
}

impl Prepared {
    pub(super) fn revalidate(&self) -> io::Result<()> {
        process_physical_work()
            .try_run_inline(
                PhysicalWorkRequest::foreground(PhysicalIoClass::Read, SCRATCH_BYTES),
                || self.check(),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error))?
    }

    fn check(&self) -> io::Result<()> {
        self.directory.revalidate()?;
        for source in &self.sources {
            source.revalidate()?;
        }
        for file in &self.files {
            if exact_entry(&self.directory, &file.name)? != Some(EntryKind::File) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            file.file.validate_revision(&file.revision)?;
        }
        Ok(())
    }
}

struct FileProof {
    name: LeafName,
    file: FileCapability,
    revision: FileRevision,
}

/// The accepted launch task retains this alongside its instance admission.
pub(super) struct Retention {
    game: ScopedDirectory,
    pending: Mutex<Option<Effect>>,
}

impl Retention {
    pub(super) fn new(game: ScopedDirectory) -> Self {
        Self {
            game,
            pending: Mutex::new(None),
        }
    }

    fn retain(&self, effect: Effect) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(
            pending.is_none(),
            "one sequential game-library effect is retained"
        );
        *pending = Some(effect);
    }

    fn has_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    fn reconcile(&self) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending = pending.take().and_then(Effect::reconcile);
        pending.is_none()
    }
}

pub(super) async fn prepare(
    inputs: GameLibraries,
    retention: Arc<Retention>,
    cancellation: &CancellationToken,
) -> Result<Prepared, LaunchError> {
    if cancellation.is_cancelled() {
        return Err(LaunchError::Cancelled);
    }
    let admission = process_physical_work()
        .admit(PhysicalWorkRequest::foreground(
            PhysicalIoClass::Write,
            SCRATCH_BYTES,
        ))
        .await
        .map_err(super::coordinator::physical_work_error)?;
    let (sender, cancelled) = transfer_cancellation_channel();
    let work = admission.run(move |_| {
        if cancelled.is_cancelled() {
            return Err(LaunchError::Cancelled);
        }
        let directory = prepare_directory(&retention)?;
        let mut files = Vec::with_capacity(inputs.sources.len());
        if inputs.sources.len() != inputs.requirements.entries().len() {
            return Err(LaunchError::InstallUnavailable);
        }
        for (source, requirement) in inputs.sources.iter().zip(inputs.requirements.entries()) {
            let name = LeafName::new(requirement.file_name())
                .map_err(|_| LaunchError::InstallUnavailable)?;
            let digest: [u8; 20] = hex::decode(requirement.sha1())
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(LaunchError::InstallUnavailable)?;
            files.push(prepare_file(
                source,
                &directory,
                name,
                requirement.size(),
                digest,
                &retention,
                cancelled.clone(),
            )?);
        }
        let prepared = Prepared {
            directory,
            sources: inputs.sources,
            files,
        };
        prepared
            .check()
            .map_err(|_| LaunchError::PreparationFailed)?;
        Ok(prepared)
    });
    tokio::pin!(work);
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => { sender.cancel(); work.await },
        result = &mut work => result,
    };
    // A stopped mutation worker cannot prove native settlement. Panic keeps
    // the accepted task's exact admission and retention visibly unsettled.
    result.expect("game-library copy worker was interrupted")
}

pub(super) async fn settle(retention: Arc<Retention>) {
    if !retention.has_pending() {
        return;
    }
    tracing::warn!(
        stage = "game_library_settlement",
        "Launch retains unresolved game-library file effects."
    );
    loop {
        if let Ok(admission) = process_physical_work()
            .admit(PhysicalWorkRequest::foreground(
                PhysicalIoClass::Write,
                SCRATCH_BYTES,
            ))
            .await
        {
            let retained = retention.clone();
            if admission
                .run(move |_| retained.reconcile())
                .await
                .expect("game-library settlement worker was interrupted")
            {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn exact_entry(directory: &ScopedDirectory, name: &LeafName) -> io::Result<Option<EntryKind>> {
    let listing = directory.entries(MAX_DIRECTORY_ENTRIES)?;
    if listing.state() != DirectoryListingState::Complete {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut matches = listing
        .entries()
        .iter()
        .filter(|entry| leaf_names_equivalent(entry.name(), name.as_os_str()));
    let Some(entry) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() || entry.name() != name.as_os_str() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(entry.kind()))
}

fn prepare_directory(retention: &Retention) -> Result<ScopedDirectory, LaunchError> {
    let name = LeafName::new("lib").expect("fixed game library directory");
    let capability = match exact_entry(&retention.game, &name)
        .map_err(|_| LaunchError::PreparationFailed)?
    {
        Some(EntryKind::Directory) => retention
            .game
            .capability()
            .open_directory(&name)
            .map_err(|_| LaunchError::PreparationFailed)?,
        Some(_) => return Err(LaunchError::PreparationFailed),
        None => match retention.game.capability().create_directory(&name) {
            DirectoryCreateOutcome::Created(directory) => directory,
            DirectoryCreateOutcome::NoEffect(_) => {
                if exact_entry(&retention.game, &name).ok() != Some(Some(EntryKind::Directory)) {
                    return Err(LaunchError::PreparationFailed);
                }
                retention
                    .game
                    .capability()
                    .open_directory(&name)
                    .map_err(|_| LaunchError::PreparationFailed)?
            }
            DirectoryCreateOutcome::AppliedUnverified(effect) => match effect.reconcile() {
                DirectoryCreateResolution::Created(directory) => directory,
                DirectoryCreateResolution::Indeterminate(effect) => {
                    retention.retain(Effect::Directory(effect));
                    return Err(LaunchError::PreparationFailed);
                }
            },
            DirectoryCreateOutcome::CreatedUnclassified { preservation, .. } => {
                if let Err(effect) = preservation.acknowledge_preserved() {
                    retention.retain(Effect::Preservation(effect));
                }
                return Err(LaunchError::PreparationFailed);
            }
        },
    };
    ScopedDirectory::from_admitted(capability, retention.game.pin().clone())
        .map_err(|_| LaunchError::PreparationFailed)
}

fn existing_file(
    directory: &ScopedDirectory,
    name: &LeafName,
    size: u64,
    digest: [u8; 20],
) -> io::Result<Option<FileProof>> {
    match exact_entry(directory, name)? {
        None => return Ok(None),
        Some(EntryKind::File) => {}
        Some(_) => return Err(io::ErrorKind::InvalidData.into()),
    }
    let file = directory.capability().open_file(name)?;
    file_proof(directory, name, file, size, digest).map(Some)
}

fn file_proof(
    directory: &ScopedDirectory,
    name: &LeafName,
    file: FileCapability,
    size: u64,
    digest: [u8; 20],
) -> io::Result<FileProof> {
    let revision = file.revision()?;
    if revision.size() != size {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut reader = file.reader(size)?;
    let mut buffer = [0; 64 << 10];
    let mut hasher = Sha1::new();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    reader.finish()?;
    if <[u8; 20]>::from(hasher.finalize()) != digest
        || exact_entry(directory, name)? != Some(EntryKind::File)
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    file.validate_revision(&revision)?;
    Ok(FileProof {
        name: name.clone(),
        file,
        revision,
    })
}

fn prepare_file(
    source: &ManagedLibraryFile,
    directory: &ScopedDirectory,
    name: LeafName,
    size: u64,
    digest: [u8; 20],
    retention: &Retention,
    cancellation: TransferCancellation,
) -> Result<FileProof, LaunchError> {
    if cancellation.is_cancelled() {
        return Err(LaunchError::Cancelled);
    }
    source
        .revalidate()
        .map_err(|_| LaunchError::InstallUnavailable)?;
    if let Some(file) =
        existing_file(directory, &name, size, digest).map_err(|_| LaunchError::PreparationFailed)?
    {
        return Ok(file);
    }
    let contract = TransferContract::authenticated_exact(
        NonZeroU64::new(size).ok_or(LaunchError::InstallUnavailable)?,
        ExpectedTransferDigests::sha1(digest),
    )
    .map_err(|_| LaunchError::InstallUnavailable)?;
    let target = CreateOnlyTransferTarget::new(
        directory
            .capability()
            .admit_transient_destination(name.clone())
            .map_err(|_| LaunchError::PreparationFailed)?,
        ManagedTransferAuthority::retain(Arc::new(directory.clone())),
    );
    match source.copy_create_only(target, contract, cancellation.clone()) {
        TransferOutcome::Complete(verified) => {
            if cancellation.is_cancelled() {
                if let Some(effect) = discard(verified.discard()) {
                    retention.retain(effect);
                }
                return Err(LaunchError::Cancelled);
            }
            let outcome = match verified.publish_create_new() {
                TransferPublicationOutcome::Pending(effect) => effect.reconcile(),
                outcome => outcome,
            };
            match outcome {
                TransferPublicationOutcome::Published { file, .. } => {
                    return file_proof(directory, &name, file, size, digest)
                        .map_err(|_| LaunchError::PreparationFailed);
                }
                TransferPublicationOutcome::NoEffect { verified, .. } => {
                    if let Some(effect) = discard(verified.discard()) {
                        retention.retain(effect);
                        return Err(LaunchError::PreparationFailed);
                    }
                }
                TransferPublicationOutcome::Pending(effect) => {
                    retention.retain(Effect::Publication(effect));
                    return Err(LaunchError::PreparationFailed);
                }
            }
        }
        TransferOutcome::Failed { report, .. } => {
            return Err(if report.last() == TransferFailureKind::Cancelled {
                LaunchError::Cancelled
            } else {
                LaunchError::PreparationFailed
            });
        }
        TransferOutcome::CleanupPending(effect) => {
            match effect.reconcile() {
                TransferCleanupResolution::Discarded { .. } => {}
                TransferCleanupResolution::Pending(effect) => {
                    retention.retain(Effect::Cleanup(effect))
                }
            }
            return Err(if cancellation.is_cancelled() {
                LaunchError::Cancelled
            } else {
                LaunchError::PreparationFailed
            });
        }
        TransferOutcome::Unsettled(effect) => {
            retention.retain(Effect::Unsettled(effect));
            return Err(LaunchError::PreparationFailed);
        }
    }
    existing_file(directory, &name, size, digest)
        .map_err(|_| LaunchError::PreparationFailed)?
        .ok_or(LaunchError::PreparationFailed)
}

enum Effect {
    Directory(DirectoryCreateObligation),
    Preservation(DirectoryCreatePreservation),
    Cleanup(TransferCleanupObligation),
    Publication(TransferPublicationObligation),
    Discard(VerifiedTransferDiscardObligation),
    Unsettled(TransferUnsettledObligation),
}

fn discard(outcome: VerifiedTransferDiscardOutcome) -> Option<Effect> {
    match outcome {
        VerifiedTransferDiscardOutcome::Discarded { .. } => None,
        VerifiedTransferDiscardOutcome::Pending(effect) => match effect.reconcile() {
            VerifiedTransferDiscardOutcome::Discarded { .. } => None,
            VerifiedTransferDiscardOutcome::Pending(effect) => Some(Effect::Discard(effect)),
        },
    }
}

impl Effect {
    fn reconcile(self) -> Option<Self> {
        match self {
            Self::Directory(effect) => match effect.reconcile() {
                DirectoryCreateResolution::Created(_) => None,
                DirectoryCreateResolution::Indeterminate(effect) => Some(Self::Directory(effect)),
            },
            Self::Preservation(effect) => {
                effect.acknowledge_preserved().err().map(Self::Preservation)
            }
            Self::Cleanup(effect) => match effect.reconcile() {
                TransferCleanupResolution::Discarded { .. } => None,
                TransferCleanupResolution::Pending(effect) => Some(Self::Cleanup(effect)),
            },
            Self::Publication(effect) => match effect.reconcile() {
                TransferPublicationOutcome::Published { .. } => None,
                TransferPublicationOutcome::NoEffect { verified, .. } => {
                    discard(verified.discard())
                }
                TransferPublicationOutcome::Pending(effect) => Some(Self::Publication(effect)),
            },
            Self::Discard(effect) => discard(effect.reconcile()),
            Self::Unsettled(effect) => effect
                .reconcile_retained_effects()
                .err()
                .map(Self::Unsettled),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::library::{LibraryLifecycle, LibraryOpenOutcome};
    use axial_minecraft::portable_path::PortableRelativePath;

    const DIGEST: [u8; 20] = [
        0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50, 0xc2,
        0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
    ];

    pub(in crate::launch) fn prepare_abc_copy(
        game: ScopedDirectory,
        source: ManagedLibraryFile,
    ) -> Prepared {
        let retention = Retention::new(game);
        let directory = prepare_directory(&retention).unwrap();
        let (_sender, cancellation) = transfer_cancellation_channel();
        let file = prepare_file(
            &source,
            &directory,
            LeafName::new("required.jar").unwrap(),
            3,
            DIGEST,
            &retention,
            cancellation,
        )
        .unwrap();
        assert!(!retention.has_pending());
        Prepared {
            directory,
            sources: vec![source],
            files: vec![file],
        }
    }

    struct Fixture {
        _library: LibraryLifecycle,
        retention: Retention,
        source: ManagedLibraryFile,
        temporary: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let temporary =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        std::fs::create_dir_all(temporary.path().join("libraries/fml")).unwrap();
        std::fs::create_dir(temporary.path().join("game")).unwrap();
        std::fs::write(temporary.path().join("libraries/fml/source.jar"), b"abc").unwrap();
        let LibraryOpenOutcome::Ready(library) = LibraryLifecycle::open(temporary.path()) else {
            panic!("isolated library admission");
        };
        let pin = library.admit().unwrap();
        let game = ScopedDirectory::from_admitted(
            pin.directory()
                .unwrap()
                .open_directory(&LeafName::new("game").unwrap())
                .unwrap(),
            pin.clone(),
        )
        .unwrap();
        let source = pin
            .managed_library()
            .unwrap()
            .observe_file(&PortableRelativePath::new_exact("libraries/fml/source.jar").unwrap())
            .unwrap()
            .unwrap();
        Fixture {
            _library: library,
            retention: Retention::new(game),
            source,
            temporary,
        }
    }

    fn copy(fixture: &Fixture, directory: &ScopedDirectory) -> Result<FileProof, LaunchError> {
        let (_sender, cancellation) = transfer_cancellation_channel();
        prepare_file(
            &fixture.source,
            directory,
            LeafName::new("required.jar").unwrap(),
            3,
            DIGEST,
            &fixture.retention,
            cancellation,
        )
    }

    #[test]
    fn copy_and_reuse_preserve_sources_unrelated_files_and_published_identity() {
        let fixture = fixture();
        let directory = prepare_directory(&fixture.retention).unwrap();
        let unrelated = fixture.temporary.path().join("game/lib/unrelated.jar");
        std::fs::write(&unrelated, b"user-owned bytes").unwrap();
        let first = copy(&fixture, &directory).unwrap();
        let second = copy(&fixture, &directory).unwrap();
        assert!(first.file.same_file(&second.file).unwrap());
        first.file.validate_revision(&first.revision).unwrap();
        assert!(!fixture.retention.has_pending());
        drop((first, second, directory));
        assert_eq!(
            std::fs::read(fixture.temporary.path().join("game/lib/required.jar")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(fixture.temporary.path().join("libraries/fml/source.jar")).unwrap(),
            b"abc"
        );
        assert_eq!(std::fs::read(unrelated).unwrap(), b"user-owned bytes");
    }

    #[test]
    fn conflicting_target_is_refused_without_overwrite() {
        let fixture = fixture();
        let directory = prepare_directory(&fixture.retention).unwrap();
        let target = fixture.temporary.path().join("game/lib/required.jar");
        std::fs::write(&target, b"abd").unwrap();
        assert!(matches!(
            copy(&fixture, &directory),
            Err(LaunchError::PreparationFailed)
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"abd");
        assert_eq!(
            std::fs::read(fixture.temporary.path().join("libraries/fml/source.jar")).unwrap(),
            b"abc"
        );
        assert!(!fixture.retention.has_pending());
    }

    #[cfg(unix)]
    #[test]
    fn matching_symlink_or_hardlink_target_is_not_adopted() {
        for symlink in [true, false] {
            let fixture = fixture();
            let directory = prepare_directory(&fixture.retention).unwrap();
            let outside = fixture.temporary.path().join("outside.jar");
            let target = fixture.temporary.path().join("game/lib/required.jar");
            std::fs::write(&outside, b"abc").unwrap();
            if symlink {
                std::os::unix::fs::symlink(&outside, &target).unwrap();
            } else {
                std::fs::hard_link(&outside, &target).unwrap();
            }
            assert!(matches!(
                copy(&fixture, &directory),
                Err(LaunchError::PreparationFailed)
            ));
            assert_eq!(std::fs::read(&outside).unwrap(), b"abc");
            assert_eq!(std::fs::read(&target).unwrap(), b"abc");
            assert_eq!(
                std::fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                symlink
            );
            assert!(!fixture.retention.has_pending());
        }
    }

    #[test]
    fn cancellation_and_changed_source_do_not_publish_a_target() {
        for cancel in [true, false] {
            let fixture = fixture();
            let directory = prepare_directory(&fixture.retention).unwrap();
            let (sender, cancellation) = transfer_cancellation_channel();
            if cancel {
                sender.cancel();
            } else {
                std::fs::write(
                    fixture.temporary.path().join("libraries/fml/source.jar"),
                    b"abd",
                )
                .unwrap();
            }
            let result = prepare_file(
                &fixture.source,
                &directory,
                LeafName::new("required.jar").unwrap(),
                3,
                DIGEST,
                &fixture.retention,
                cancellation,
            );
            if cancel {
                assert!(matches!(result, Err(LaunchError::Cancelled)));
            } else {
                assert!(matches!(result, Err(LaunchError::InstallUnavailable)));
            }
            assert!(
                !fixture
                    .temporary
                    .path()
                    .join("game/lib/required.jar")
                    .exists()
            );
            assert!(!fixture.retention.has_pending());
            assert!(fixture.retention.reconcile());
        }
    }

    #[test]
    fn published_capability_cannot_adopt_a_same_bytes_path_replacement() {
        let fixture = fixture();
        let directory = prepare_directory(&fixture.retention).unwrap();
        let name = LeafName::new("required.jar").unwrap();
        let target = CreateOnlyTransferTarget::new(
            directory
                .capability()
                .admit_transient_destination(name.clone())
                .unwrap(),
            ManagedTransferAuthority::retain(Arc::new(directory.clone())),
        );
        let contract = TransferContract::authenticated_exact(
            NonZeroU64::new(3).unwrap(),
            ExpectedTransferDigests::sha1(DIGEST),
        )
        .unwrap();
        let (_sender, cancellation) = transfer_cancellation_channel();
        let TransferOutcome::Complete(verified) =
            fixture
                .source
                .copy_create_only(target, contract, cancellation)
        else {
            panic!("isolated source did not copy");
        };
        let TransferPublicationOutcome::Published { file, .. } = verified.publish_create_new()
        else {
            panic!("isolated copy did not publish");
        };
        let target = fixture.temporary.path().join("game/lib/required.jar");
        let preserved = fixture.temporary.path().join("game/lib/preserved.jar");
        std::fs::rename(&target, &preserved).unwrap();
        std::fs::write(&target, b"abc").unwrap();
        assert!(file_proof(&directory, &name, file, 3, DIGEST).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        assert_eq!(std::fs::read(&preserved).unwrap(), b"abc");
        assert_eq!(
            std::fs::read(fixture.temporary.path().join("libraries/fml/source.jar")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn prepared_copy_rejects_same_bytes_replacement_before_spawn() {
        let fixture = fixture();
        let directory = prepare_directory(&fixture.retention).unwrap();
        let file = copy(&fixture, &directory).unwrap();
        let prepared = Prepared {
            directory,
            sources: vec![fixture.source],
            files: vec![file],
        };
        prepared.revalidate().unwrap();
        let target = fixture.temporary.path().join("game/lib/required.jar");
        let preserved = fixture.temporary.path().join("game/lib/preserved.jar");
        std::fs::rename(&target, &preserved).unwrap();
        std::fs::write(&target, b"abc").unwrap();
        assert!(prepared.revalidate().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        assert_eq!(std::fs::read(&preserved).unwrap(), b"abc");
        assert!(!fixture.retention.has_pending());
    }
}
