use super::{
    MUSIC_FILES, MUSIC_MAX_BYTES, MusicError, MusicService, MusicStatusResponse, MusicTrackStatus,
};
use crate::library::ApplicationRootPin;
use crate::tasks::CancellationToken;
use axial_fs::{
    Directory, DirectoryCreateObligation, DirectoryCreateOutcome, DirectoryCreatePreservation,
    DirectoryCreateResolution, DirectoryListingState, EntryKind, LeafName, leaf_names_equivalent,
};
use axial_minecraft::download::{
    TransferCleanupObligation, TransferCleanupResolution, TransferFailureKind, TransferOutcome,
    TransferPublicationObligation, TransferPublicationOutcome, TransferUnsettledObligation,
    VerifiedCreateOnly, VerifiedTransferDiscardObligation, VerifiedTransferDiscardOutcome,
};
use std::io;

pub(super) fn track_name(index: usize) -> LeafName {
    LeafName::new(MUSIC_FILES[index]).expect("fixed music leaf")
}
fn music_name() -> LeafName {
    LeafName::new("music").expect("fixed music directory")
}

fn directory(pin: &ApplicationRootPin) -> Result<Option<Directory>, MusicError> {
    let root = pin.directory().map_err(|_| MusicError::Unavailable)?;
    match root.open_directory(&music_name()) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(MusicError::DownloadFailed),
    }
}

pub(super) fn status(pin: &ApplicationRootPin) -> MusicStatusResponse {
    let directory = directory(pin).ok().flatten();
    let tracks = MUSIC_FILES
        .iter()
        .enumerate()
        .map(|(index, file)| MusicTrackStatus {
            file: (*file).into(),
            cached: directory.as_ref().is_some_and(|directory| {
                let name = track_name(index);
                exact_binding(directory, &name).ok() == Some(true)
                    && directory.open_file(&name).ok().is_some_and(|file| {
                        file.revision().is_ok_and(|revision| {
                            revision.size() <= MUSIC_MAX_BYTES
                                && file.validate_revision(&revision).is_ok()
                        })
                    })
            }),
        })
        .collect();
    MusicStatusResponse {
        tracks,
        count: MUSIC_FILES.len(),
    }
}

pub(super) fn read(pin: &ApplicationRootPin, index: usize) -> Result<Option<Vec<u8>>, MusicError> {
    let Some(directory) = directory(pin)? else {
        return Ok(None);
    };
    let name = track_name(index);
    if !exact_binding(&directory, &name).map_err(|_| MusicError::DownloadFailed)? {
        return Ok(None);
    }
    let file = directory
        .open_file(&name)
        .map_err(|_| MusicError::DownloadFailed)?;
    let bytes = file
        .read_bounded(MUSIC_MAX_BYTES)
        .map_err(|_| MusicError::DownloadFailed)?;
    if !exact_binding(&directory, &name).map_err(|_| MusicError::DownloadFailed)? {
        return Err(MusicError::DownloadFailed);
    }
    Ok(Some(bytes))
}

pub(super) fn prepare_directory(
    service: &MusicService,
    pin: &ApplicationRootPin,
) -> Result<Directory, MusicError> {
    let _creation = service
        .shared
        .directory_creation
        .lock()
        .map_err(|_| MusicError::Unavailable)?;
    if let Some(directory) = directory(pin)? {
        return Ok(directory);
    }
    let root = pin.directory().map_err(|_| MusicError::Unavailable)?;
    match root.create_directory(&music_name()) {
        DirectoryCreateOutcome::Created(directory) => Ok(directory),
        DirectoryCreateOutcome::NoEffect(_) => directory(pin)?.ok_or(MusicError::DownloadFailed),
        DirectoryCreateOutcome::AppliedUnverified(effect) => match effect.reconcile() {
            DirectoryCreateResolution::Created(directory) => Ok(directory),
            DirectoryCreateResolution::Indeterminate(effect) => {
                service.retain_effect(pin.clone(), Effect::Directory(effect));
                Err(MusicError::Unavailable)
            }
        },
        DirectoryCreateOutcome::CreatedUnclassified { preservation, .. } => {
            if let Err(preservation) = preservation.acknowledge_preserved() {
                service.retain_effect(pin.clone(), Effect::Preservation(preservation));
            }
            Err(MusicError::Unavailable)
        }
    }
}

fn exact_binding(directory: &Directory, name: &LeafName) -> io::Result<bool> {
    let listing = directory.entries(16)?;
    if listing.state() != DirectoryListingState::Complete {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut equivalent = listing
        .entries()
        .iter()
        .filter(|entry| leaf_names_equivalent(entry.name(), name.as_os_str()));
    let Some(entry) = equivalent.next() else {
        return Ok(false);
    };
    if equivalent.next().is_some()
        || entry.name() != name.as_os_str()
        || entry.kind() != EntryKind::File
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(true)
}

pub(super) fn finish(
    service: &MusicService,
    pin: &ApplicationRootPin,
    index: usize,
    outcome: TransferOutcome<VerifiedCreateOnly>,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, MusicError> {
    match outcome {
        TransferOutcome::Complete(verified) => {
            if cancellation.is_cancelled() {
                discard(service, pin.clone(), verified.discard())?;
                return Err(MusicError::Unavailable);
            }
            let outcome = match verified.publish_create_new() {
                TransferPublicationOutcome::Pending(effect) => effect.reconcile(),
                outcome => outcome,
            };
            match outcome {
                TransferPublicationOutcome::Published { .. } => {}
                TransferPublicationOutcome::NoEffect { verified, .. } => {
                    discard(service, pin.clone(), verified.discard())?
                }
                TransferPublicationOutcome::Pending(effect) => {
                    service.retain_effect(pin.clone(), Effect::Publication(effect));
                    return Err(MusicError::Unavailable);
                }
            }
            read(pin, index)?.ok_or(MusicError::NotFound)
        }
        TransferOutcome::Failed { report, .. } => {
            #[cfg(test)]
            eprintln!("music transfer did not publish: {report:?}");
            Err(if report.last() == TransferFailureKind::Cancelled {
                MusicError::Unavailable
            } else {
                MusicError::DownloadFailed
            })
        }
        TransferOutcome::CleanupPending(effect) => match effect.reconcile() {
            TransferCleanupResolution::Discarded { report, .. } => {
                Err(if report.last() == TransferFailureKind::Cancelled {
                    MusicError::Unavailable
                } else {
                    MusicError::DownloadFailed
                })
            }
            TransferCleanupResolution::Pending(effect) => {
                service.retain_effect(pin.clone(), Effect::Cleanup(effect));
                Err(MusicError::Unavailable)
            }
        },
        TransferOutcome::Unsettled(effect) => {
            service.retain_effect(pin.clone(), Effect::Unsettled(effect));
            Err(MusicError::Unavailable)
        }
    }
}

fn discard(
    service: &MusicService,
    pin: ApplicationRootPin,
    outcome: VerifiedTransferDiscardOutcome,
) -> Result<(), MusicError> {
    match discard_pending(outcome) {
        None => Ok(()),
        Some(effect) => {
            service.retain_effect(pin, effect);
            Err(MusicError::Unavailable)
        }
    }
}

fn discard_pending(outcome: VerifiedTransferDiscardOutcome) -> Option<Effect> {
    match outcome {
        VerifiedTransferDiscardOutcome::Discarded { .. } => None,
        VerifiedTransferDiscardOutcome::Pending(effect) => match effect.reconcile() {
            VerifiedTransferDiscardOutcome::Discarded { .. } => None,
            VerifiedTransferDiscardOutcome::Pending(effect) => Some(Effect::Discard(effect)),
        },
    }
}

pub(super) struct RetainedEffect {
    pub pin: ApplicationRootPin,
    pub effect: Effect,
}

pub(super) enum Effect {
    Directory(DirectoryCreateObligation),
    Preservation(DirectoryCreatePreservation),
    Cleanup(TransferCleanupObligation),
    Publication(TransferPublicationObligation),
    Discard(VerifiedTransferDiscardObligation),
    Unsettled(TransferUnsettledObligation),
}

impl RetainedEffect {
    pub(super) fn reconcile(self) -> Option<Self> {
        let pending = match self.effect {
            Effect::Directory(effect) => match effect.reconcile() {
                DirectoryCreateResolution::Created(_) => None,
                DirectoryCreateResolution::Indeterminate(effect) => Some(Effect::Directory(effect)),
            },
            Effect::Preservation(effect) => effect
                .acknowledge_preserved()
                .err()
                .map(Effect::Preservation),
            Effect::Cleanup(effect) => match effect.reconcile() {
                TransferCleanupResolution::Discarded { .. } => None,
                TransferCleanupResolution::Pending(effect) => Some(Effect::Cleanup(effect)),
            },
            Effect::Publication(effect) => match effect.reconcile() {
                TransferPublicationOutcome::Published { .. } => None,
                TransferPublicationOutcome::NoEffect { verified, .. } => {
                    discard_pending(verified.discard())
                }
                TransferPublicationOutcome::Pending(effect) => Some(Effect::Publication(effect)),
            },
            Effect::Discard(effect) => match effect.reconcile() {
                VerifiedTransferDiscardOutcome::Discarded { .. } => None,
                VerifiedTransferDiscardOutcome::Pending(effect) => Some(Effect::Discard(effect)),
            },
            Effect::Unsettled(effect) => effect
                .reconcile_retained_effects()
                .err()
                .map(Effect::Unsettled),
        };
        pending.map(|effect| Self {
            pin: self.pin,
            effect,
        })
    }
}
