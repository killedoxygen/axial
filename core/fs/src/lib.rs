macro_rules! impl_error_accessor {
    ($type:ty) => {
        impl $type {
            pub fn error(&self) -> &std::io::Error {
                &self.error
            }
        }
    };
}

macro_rules! impl_redacted_debug {
    ($type:ty) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_struct(stringify!($type))
                    .finish_non_exhaustive()
            }
        }
    };
}

mod authority;
mod directory;
mod durable;
mod file;
mod moves;
mod names;
mod owner;
mod park;
mod platform;
mod recoverable;
mod recovery;
mod reset;
mod root;
mod stage;
mod transient;

use authority::*;
pub use directory::*;
pub use durable::*;
pub use file::*;
pub use moves::*;
pub use names::*;
pub use owner::*;
pub use park::*;
use recoverable::*;
pub use reset::PendingRootReset;
pub use root::*;
pub use stage::*;

#[cfg(test)]
fn test_tempdir() -> std::io::Result<tempfile::TempDir> {
    // Root admission intentionally rejects symlink ancestry. macOS commonly
    // spells its temporary parent through /var, so fixture roots must start at
    // the physical parent. Individual alias/symlink tests still create theirs.
    tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir())?)
}

pub use transient::{
    TransientCreationObligation, TransientDestination, TransientDestinationBatch,
    TransientDestinationCancelObligation, TransientDestinationCancelOutcome,
    TransientDiscardObligation, TransientDiscardOutcome, TransientPublicationBatch,
    TransientPublicationBatchCreateFailure, TransientPublicationBatchObligation,
    TransientPublicationBatchOutcome, TransientPublicationMember, TransientStage,
    TransientStageCreateOutcome, TransientStageSealFailure, TransientStageSealed,
};

use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::ControlFlow;
#[cfg(unix)]
use std::path::Component;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use recovery::{
    RecoveryJournal, RecoveryName, RecoveryPhase, RecoveryRecord, RecoveryRegistration,
    recovery_park_leaf, recovery_stage_leaf,
};

pub const ROOT_LEASE_NAME: &str = ".axial-root.lease";
const MAX_LEAF_UNITS: usize = 255;
const MAX_STAGE_ATTEMPTS: usize = 32;
pub const MAX_DIRECTORY_LIST_ENTRIES: usize = 100_000;
pub const MAX_REVISION_DEPTH: usize = 256;
const MAX_OUTSTANDING_EFFECTS: usize = 512;
const MAX_FILE_RANGE_BYTES: usize = 128 * 1024;

/// An effect attempt classified at its commit boundary: applied and verified, refused
/// before any effect with its input returned, or applied without verification and
/// retained by an obligation that must be reconciled.
#[must_use = "filesystem effect outcomes must be explicitly settled"]
#[derive(Debug)]
pub enum Outcome<A, N, O> {
    Applied(A),
    NoEffect { error: io::Error, item: N },
    AppliedUnverified(O),
}

/// The reconciliation of an unverified effect, keeping the same three-way
/// classification.
#[must_use = "filesystem effect resolutions must be handled"]
#[derive(Debug)]
pub enum Resolution<A, N, O> {
    Applied(A),
    NoEffect(N),
    Indeterminate(O),
}

/// An unverified effect obligation that reconciles into its outcome's classification.
pub trait Reconcile<A, N>: Sized {
    fn reconcile(self) -> Resolution<A, N, Self>;
}

impl<A, N, O: Reconcile<A, N>> Outcome<A, N, O> {
    /// Reconciles an unverified attempt once. `NoEffect` keeps the attempt's
    /// refusal error; a no-effect found by reconciliation carries none.
    pub fn reconcile(self) -> Resolution<A, (Option<io::Error>, N), O> {
        match self {
            Self::Applied(applied) => Resolution::Applied(applied),
            Self::NoEffect { error, item } => Resolution::NoEffect((Some(error), item)),
            Self::AppliedUnverified(obligation) => match obligation.reconcile() {
                Resolution::Applied(applied) => Resolution::Applied(applied),
                Resolution::NoEffect(item) => Resolution::NoEffect((None, item)),
                Resolution::Indeterminate(obligation) => Resolution::Indeterminate(obligation),
            },
        }
    }
}

macro_rules! impl_reconcile {
    ($($obligation:ty: $applied:ty, $no_effect:ty;)*) => {$(
        impl Reconcile<$applied, $no_effect> for $obligation {
            fn reconcile(self) -> Resolution<$applied, $no_effect, Self> {
                <$obligation>::reconcile(self)
            }
        }
    )*};
}

impl_reconcile! {
    FileMoveObligation: FileCapability, FileCapability;
    DirectoryMoveObligation: Directory, Directory;
    FileRemovalObligation: (), ParkedFile;
    FileRestoreObligation: FileCapability, ParkedFile;
    DirectoryParkObligation: ParkedDirectory, Directory;
    DirectoryRemovalObligation: (), ParkedDirectory;
    DirectoryRestoreObligation: Directory, ParkedDirectory;
}

impl Reconcile<FileCapability, SealedStagedFile> for Box<FilePromotionObligation> {
    fn reconcile(self) -> FilePromotionResolution {
        (*self).reconcile()
    }
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

fn stale_capability() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "filesystem capability has been revoked",
    )
}

fn identity_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
