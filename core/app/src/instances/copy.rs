//! Shared capability-to-capability payload copying for instance publication.
//! Callers choose the retained subtree policy; every file gets independent bytes.

use super::{
    create::{InstanceService, NativeEffect},
    model::{InstanceError, InstanceResult},
};
use crate::{
    files::{PortableName, ScopedDirectory, StageCreateOutcome},
    tasks::CancellationToken,
};
use std::collections::HashSet;

pub(crate) struct CopyBudget {
    pub(crate) entries: usize,
    pub(crate) bytes: u64,
    pub(crate) file_bytes: u64,
}

pub(crate) fn check_cancel(cancel: &CancellationToken) -> InstanceResult<()> {
    if cancel.is_cancelled() {
        Err(InstanceError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn copy_tree(
    service: &InstanceService,
    source: &axial_fs::Directory,
    destination: &ScopedDirectory,
    cancel: &CancellationToken,
    budget: &mut CopyBudget,
    depth: usize,
) -> InstanceResult<()> {
    copy_tree_excluding(service, source, destination, cancel, budget, depth, |_| {
        false
    })
}

/// Exclusions apply only to this directory, not to nested user directories.
pub(crate) fn copy_tree_excluding(
    service: &InstanceService,
    source: &axial_fs::Directory,
    destination: &ScopedDirectory,
    cancel: &CancellationToken,
    budget: &mut CopyBudget,
    depth: usize,
    excluded: fn(&str) -> bool,
) -> InstanceResult<()> {
    if depth > 64 {
        return Err(InstanceError::InvalidInput);
    }
    let revision = source
        .revision()
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let listing = source
        .entries(budget.entries.min(axial_fs::MAX_DIRECTORY_LIST_ENTRIES))
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    if listing.state() != axial_fs::DirectoryListingState::Complete {
        return Err(InstanceError::InvalidInput);
    }
    let mut portable = HashSet::new();
    for entry in listing.entries() {
        check_cancel(cancel)?;
        budget.entries = budget
            .entries
            .checked_sub(1)
            .ok_or(InstanceError::InvalidInput)?;
        let name = PortableName::new_exact(entry.utf8_name().ok_or(InstanceError::InvalidInput)?)
            .map_err(|_| InstanceError::InvalidInput)?;
        if !portable.insert(name.key()) {
            return Err(InstanceError::InvalidInput);
        }
        if excluded(name.as_str()) {
            continue;
        }
        match entry.kind() {
            axial_fs::EntryKind::Directory => {
                let child = source
                    .open_observed_directory(entry)
                    .map_err(|_| InstanceError::DirectoryUnavailable)?;
                let target = service.fresh_directory(destination, &name)?;
                copy_tree(service, &child, &target, cancel, budget, depth + 1)?;
            }
            axial_fs::EntryKind::File => {
                copy_file(service, source, destination, &name, cancel, budget, false)?
            }
            _ => return Err(InstanceError::DirectoryUnavailable),
        }
    }
    source
        .validate_revision(&revision)
        .map_err(|_| InstanceError::Conflict)
}

pub(crate) fn copy_file(
    service: &InstanceService,
    source: &axial_fs::Directory,
    destination: &ScopedDirectory,
    name: &PortableName,
    cancel: &CancellationToken,
    budget: &mut CopyBudget,
    optional: bool,
) -> InstanceResult<()> {
    check_cancel(cancel)?;
    let leaf = axial_fs::LeafName::new(name.as_str()).map_err(|_| InstanceError::InvalidInput)?;
    let file = match source.open_file(&leaf) {
        Ok(file) => file,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(InstanceError::DirectoryUnavailable),
    };
    let revision = file
        .revision()
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let limit = budget.bytes.min(budget.file_bytes);
    let mut reader = file
        .reader(limit)
        .map_err(|_| InstanceError::DirectoryUnavailable)?;
    let mut stage = match destination.stage(name) {
        StageCreateOutcome::Created(stage) => stage,
        StageCreateOutcome::NoEffect(_) => return Err(InstanceError::DirectoryUnavailable),
        unresolved => {
            service.retain(NativeEffect::StageCreate(unresolved));
            return Err(InstanceError::SettlementRequired);
        }
    };
    let copied = stage
        .copy_from(
            &mut CancellationReader {
                inner: &mut reader,
                cancel,
            },
            limit,
        )
        .and_then(|bytes| reader.finish().map(|()| bytes))
        .and_then(|bytes| file.validate_revision(&revision).map(|()| bytes));
    let bytes = match copied {
        Ok(bytes) if !cancel.is_cancelled() => bytes,
        result => {
            let (discard, pin) = stage.discard().into_parts();
            if !matches!(discard, axial_fs::StageDiscardOutcome::Discarded) {
                service.retain(NativeEffect::StageDiscard(discard, pin));
                return Err(InstanceError::SettlementRequired);
            }
            return Err(if result.is_ok() || cancel.is_cancelled() {
                InstanceError::Cancelled
            } else {
                InstanceError::DirectoryUnavailable
            });
        }
    };
    let sealed = match stage.seal() {
        Ok(sealed) => sealed,
        Err(error) => {
            service.retain(NativeEffect::StageSeal(error));
            return Err(InstanceError::SettlementRequired);
        }
    };
    let (outcome, pin) = sealed.publish().into_parts();
    match outcome {
        axial_fs::FilePromotionOutcome::Applied(_) => {
            budget.bytes -= bytes;
            Ok(())
        }
        unresolved => {
            service.retain(NativeEffect::FilePublish(unresolved, pin));
            Err(InstanceError::SettlementRequired)
        }
    }
}

struct CancellationReader<'a, R> {
    inner: R,
    cancel: &'a CancellationToken,
}
impl<R: std::io::Read> std::io::Read for CancellationReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "instance copy cancelled",
            ));
        }
        self.inner.read(bytes)
    }
}
