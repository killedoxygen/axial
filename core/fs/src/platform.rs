use crate::EntryKind;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::char::{canonical_combining_class, decompose_canonical};

use crate::recovery::RECOVERY_CONTROL_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BindingState {
    Absent,
    Exact,
    Occupied,
}

#[derive(Debug)]
pub(crate) struct RecoveryControlSyncError {
    error: io::Error,
    barrier_confirmed: bool,
}

impl RecoveryControlSyncError {
    fn before_barrier(error: io::Error) -> Self {
        Self {
            error,
            barrier_confirmed: false,
        }
    }

    fn after_barrier(error: io::Error) -> Self {
        Self {
            error,
            barrier_confirmed: true,
        }
    }

    pub(crate) fn into_error(self) -> io::Error {
        self.error
    }

    pub(crate) fn into_error_and_barrier_state(self) -> (io::Error, bool) {
        (self.error, self.barrier_confirmed)
    }
}

fn recovery_control_read_exact_with(
    offset: u64,
    bytes: &mut [u8],
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
) -> io::Result<()> {
    let mut filled = 0_usize;
    while filled < bytes.len() {
        let position = offset
            .checked_add(u64::try_from(filled).map_err(|_| {
                io::Error::other("recovery control read position is not representable")
            })?)
            .ok_or_else(|| io::Error::other("recovery control offset overflowed"))?;
        match read_at(&mut bytes[filled..], position) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "recovery control ended before the requested range",
                ));
            }
            Ok(read) => {
                filled = filled
                    .checked_add(read)
                    .ok_or_else(|| io::Error::other("recovery control read length overflowed"))?
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn recovery_control_write_all_with(
    offset: u64,
    bytes: &[u8],
    mut write_at: impl FnMut(&[u8], u64) -> io::Result<usize>,
) -> io::Result<()> {
    let mut written = 0_usize;
    while written < bytes.len() {
        let position = offset
            .checked_add(u64::try_from(written).map_err(|_| {
                io::Error::other("recovery control write position is not representable")
            })?)
            .ok_or_else(|| io::Error::other("recovery control offset overflowed"))?;
        match write_at(&bytes[written..], position) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => {
                written = written
                    .checked_add(count)
                    .ok_or_else(|| io::Error::other("recovery control write length overflowed"))?
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct PublicationReceipt {
    state: PublicationReceiptState,
    binding: PublicationBinding,
}

#[derive(Clone, Copy)]
enum PublicationReceiptState {
    Attempted,
    #[cfg(unix)]
    ParentsPending,
    #[cfg(unix)]
    Poisoned,
    #[cfg(windows)]
    Reported,
    Committed,
}

#[derive(Clone, Eq, PartialEq)]
struct PublicationBinding {
    // This revision fence assumes the staged handle remains under cooperative ownership. It is
    // not a cryptographic proof against an external same-size, timestamp-restoring writer.
    attempt_id: u64,
    staged_file: Identity,
    sealed_size: u64,
    sealed_stamp: FileStamp,
    source_parent: Identity,
    source_name: OsString,
    destination_parent: Identity,
    destination_name: OsString,
}

impl PublicationReceipt {
    pub(crate) fn is_attempted(&self) -> bool {
        matches!(self.state, PublicationReceiptState::Attempted)
    }

    fn attempted(
        attempt_id: u64,
        staged_file: Identity,
        sealed_size: u64,
        sealed_stamp: FileStamp,
        source_parent: Identity,
        source_name: &OsStr,
        destination_parent: Identity,
        destination_name: &OsStr,
    ) -> Self {
        Self {
            state: PublicationReceiptState::Attempted,
            binding: PublicationBinding {
                attempt_id,
                staged_file,
                sealed_size,
                sealed_stamp,
                source_parent,
                source_name: source_name.to_os_string(),
                destination_parent,
                destination_name: destination_name.to_os_string(),
            },
        }
    }

    #[cfg(unix)]
    fn mark_parents_pending(&mut self) {
        debug_assert!(matches!(self.state, PublicationReceiptState::Attempted));
        self.state = PublicationReceiptState::ParentsPending;
    }

    #[cfg(windows)]
    fn mark_reported(&mut self) {
        debug_assert!(matches!(self.state, PublicationReceiptState::Attempted));
        self.state = PublicationReceiptState::Reported;
    }

    #[cfg(unix)]
    fn mark_poisoned(&mut self) {
        debug_assert!(matches!(
            self.state,
            PublicationReceiptState::Attempted
                | PublicationReceiptState::ParentsPending
                | PublicationReceiptState::Poisoned
        ));
        self.state = PublicationReceiptState::Poisoned;
    }

    fn mark_committed(&mut self) {
        self.state = PublicationReceiptState::Committed;
    }

    fn binding(&self) -> &PublicationBinding {
        &self.binding
    }

    pub(crate) fn matches_attempt(&self, attempt_id: u64) -> bool {
        self.binding().attempt_id == attempt_id
    }

    pub(crate) fn accepts_successor(&self, successor: &Self) -> bool {
        if self.binding() != successor.binding() {
            return false;
        }
        match (self.state, successor.state) {
            (PublicationReceiptState::Attempted, PublicationReceiptState::Attempted) => true,
            #[cfg(unix)]
            (
                PublicationReceiptState::Attempted,
                PublicationReceiptState::ParentsPending
                | PublicationReceiptState::Poisoned
                | PublicationReceiptState::Committed,
            ) => true,
            #[cfg(windows)]
            (PublicationReceiptState::Attempted, PublicationReceiptState::Reported) => true,
            #[cfg(unix)]
            (
                PublicationReceiptState::ParentsPending,
                PublicationReceiptState::ParentsPending
                | PublicationReceiptState::Poisoned
                | PublicationReceiptState::Committed,
            ) => true,
            #[cfg(unix)]
            (PublicationReceiptState::Poisoned, PublicationReceiptState::Poisoned) => true,
            #[cfg(windows)]
            (
                PublicationReceiptState::Reported,
                PublicationReceiptState::Reported | PublicationReceiptState::Committed,
            ) => true,
            (PublicationReceiptState::Committed, PublicationReceiptState::Committed) => true,
            _ => false,
        }
    }

    pub(crate) fn validate_recovery_binding(
        &self,
        attempt_id: u64,
        staged_file: &File,
        parent: &DirectoryHandle,
        source_name: &OsStr,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        Self::validate_binding(
            self.binding(),
            attempt_id,
            file_identity(staged_file)?,
            parent,
            source_name,
            parent,
            destination_name,
        )?;
        let (size, stamp) = file_receipt_fields(staged_file)?;
        Self::validate_content_observation(self.binding(), size, stamp)
    }

    fn validate_binding(
        binding: &PublicationBinding,
        attempt_id: u64,
        staged_file: Identity,
        source_parent: &DirectoryHandle,
        source_name: &OsStr,
        destination_parent: &DirectoryHandle,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        if attempt_id != binding.attempt_id
            || staged_file != binding.staged_file
            || directory_identity(source_parent)? != binding.source_parent
            || source_name != binding.source_name.as_os_str()
            || directory_identity(destination_parent)? != binding.destination_parent
            || destination_name != binding.destination_name.as_os_str()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "named publication receipt does not match this mutation",
            ));
        }
        Ok(())
    }

    fn validate_exact_revision(binding: &PublicationBinding, staged_file: &File) -> io::Result<()> {
        let (size, stamp) = file_receipt_fields(staged_file)?;
        if size != binding.sealed_size || stamp != binding.sealed_stamp {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged file revision changed after publication preparation",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn validate_content(binding: &PublicationBinding, staged_file: &File) -> io::Result<()> {
        let (size, stamp) = file_receipt_fields(staged_file)?;
        Self::validate_content_observation(binding, size, stamp)
    }

    fn validate_content_observation(
        binding: &PublicationBinding,
        size: u64,
        stamp: FileStamp,
    ) -> io::Result<()> {
        if size != binding.sealed_size || !file_content_stamp_matches(stamp, binding.sealed_stamp) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged file content changed after publication preparation",
            ));
        }
        Ok(())
    }
}

pub(crate) fn prepare_publication(
    attempt_id: u64,
    staged_file: &File,
    sealed_size: u64,
    sealed_stamp: FileStamp,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<PublicationReceipt> {
    let (observed_size, observed_stamp) = file_receipt_fields(staged_file)?;
    if observed_size != sealed_size || observed_stamp != sealed_stamp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged file revision changed before publication preparation",
        ));
    }
    let staged_file = file_identity(staged_file)?;
    if file_binding_state(source_parent, source_name, staged_file)? != BindingState::Exact {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged file binding changed before publication preparation",
        ));
    }
    Ok(PublicationReceipt::attempted(
        attempt_id,
        staged_file,
        sealed_size,
        sealed_stamp,
        directory_identity(source_parent)?,
        source_name,
        directory_identity(destination_parent)?,
        destination_name,
    ))
}

fn validate_applied_publication(
    staged_file: Identity,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    if file_binding_state(source_parent, source_name, staged_file)? != BindingState::Absent
        || file_binding_state(destination_parent, destination_name, staged_file)?
            != BindingState::Exact
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named publication topology is not applied",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransientPublicationState {
    Unpublished,
    Published,
    Indeterminate,
}

pub(crate) struct DirectoryEntries {
    pub(crate) entries: Vec<(OsString, EntryKind)>,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VisitCompletion {
    Complete,
    Stopped,
    LimitExceeded,
}

pub(crate) enum CreateDirectoryError {
    NoEffect(io::Error),
    #[cfg_attr(
        windows,
        expect(
            dead_code,
            reason = "Windows retains an exact handle for every unclassified directory creation"
        )
    )]
    CreatedUnclassified(io::Error),
    #[cfg(windows)]
    AppliedUnverified {
        error: io::Error,
        retained: DirectoryHandle,
    },
}

pub(crate) enum CreateFileError {
    NoEffect(io::Error),
    AppliedUnverified { error: io::Error, retained: File },
}

pub(crate) enum ParkFileError {
    NoEffect(io::Error),
    AppliedUnverified(io::Error),
}

pub(crate) enum ParkDirectoryError {
    NoEffect(io::Error),
    AppliedUnverified(io::Error),
}

/// Bytes allotted to one equivalence key of a bounded leaf name.
pub(crate) const MAX_EQUIVALENCE_KEY_BYTES: usize = 1 + crate::MAX_LEAF_UNITS * 16;

/// Appends the one portable leaf-name equivalence key: full case folding followed by
/// canonical decomposition. Two names are portable aliases exactly when their keys
/// are equal. `bounded` keeps the key and scratch within their reserved capacity and
/// fails rather than allocating.
pub(crate) fn write_portable_key(
    name: &str,
    key: &mut Vec<u8>,
    normalization: &mut Vec<(u8, char)>,
    bounded: bool,
) -> io::Result<()> {
    normalization.clear();
    let mut exhausted = false;
    'folded: for character in name.case_fold() {
        decompose_canonical(character, |decomposed| {
            if bounded && normalization.len() == normalization.capacity() {
                exhausted = true;
                return;
            }
            let class = canonical_combining_class(decomposed);
            normalization.push((class, decomposed));
            if class == 0 {
                return;
            }
            let mut index = normalization.len() - 1;
            while index > 0 && normalization[index - 1].0 > class {
                normalization.swap(index - 1, index);
                index -= 1;
            }
        });
        if exhausted {
            break 'folded;
        }
    }
    if exhausted {
        return Err(io::ErrorKind::InvalidData.into());
    }
    for (_, character) in normalization.iter().copied() {
        let mut encoded = [0_u8; 4];
        let bytes = character.encode_utf8(&mut encoded).as_bytes();
        if bounded {
            extend_preallocated_key(key, bytes)?;
        } else {
            key.extend_from_slice(bytes);
        }
    }
    Ok(())
}

pub(crate) fn portable_key(name: &str) -> Vec<u8> {
    let mut key = Vec::new();
    write_portable_key(name, &mut key, &mut Vec::new(), false)
        .expect("unbounded portable key cannot exhaust its capacity");
    key
}

pub(crate) fn leaf_names_equal(first: &OsStr, second: &OsStr) -> bool {
    if first == second {
        return true;
    }
    if let (Some(first), Some(second)) = (first.to_str(), second.to_str())
        && portable_key(first) == portable_key(second)
    {
        return true;
    }
    native::leaf_names_equal_native(first, second)
}

pub(crate) fn leaf_name_equivalence_keys(name: &OsStr) -> Vec<Vec<u8>> {
    let mut keys = Vec::with_capacity(2);
    if let Some(name) = name.to_str() {
        let mut key = vec![b'p'];
        write_portable_key(name, &mut key, &mut Vec::new(), false)
            .expect("unbounded portable key cannot exhaust its capacity");
        keys.push(key);
    }
    let native = native::leaf_name_native_key(name);
    if !keys.contains(&native) {
        keys.push(native);
    }
    keys
}

/// Fills both lookup keys for `name` within their reserved capacity. Returns whether
/// the name has a portable key.
pub(crate) fn fill_leaf_name_equivalence_keys(
    name: &OsStr,
    portable: &mut Vec<u8>,
    native: &mut Vec<u8>,
    normalization: &mut Vec<(u8, char)>,
) -> io::Result<bool> {
    portable.clear();
    native.clear();
    let has_portable = if let Some(name) = name.to_str() {
        extend_preallocated_key(portable, b"p")?;
        write_portable_key(name, portable, normalization, true)?;
        true
    } else {
        false
    };
    extend_preallocated_key(native, b"n")?;
    native::fill_leaf_name_native_key(name, native)?;
    Ok(has_portable)
}

fn extend_preallocated_key(key: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if key.capacity().saturating_sub(key.len()) < bytes.len() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    key.extend_from_slice(bytes);
    Ok(())
}

pub(crate) fn birth_time_ns(seconds: i64, nanos: i64) -> io::Result<u64> {
    if seconds == 0 && nanos == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem birth time is unavailable",
        ));
    }
    let seconds = u64::try_from(seconds).map_err(|_| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "birth time precedes the Unix epoch",
        )
    })?;
    let nanos = u64::try_from(nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "birth time nanos are invalid")
        })?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(nanos))
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "birth time overflowed"))
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as native;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as native;

pub(crate) use native::*;
