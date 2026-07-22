use axial_fs::{
    Directory, DirectoryCreateOutcome, DirectoryCreateResolution, DirectoryEntry, DirectoryIdentity,
    DirectoryListing, DirectoryMoveOutcome, DirectoryMoveReceipt, DirectoryMoveReceiptOutcome,
    DirectoryMoveResolution, DirectoryParkOutcome, DirectoryParkResolution, DirectoryRemovalOutcome,
    DirectoryRemovalResolution, EffectOwner, EffectOwnerRetentionError, EntryKind,
    ExpectedFileContent, FileCapability, FileCreateOutcome, FileCreateResolution, FileMoveOutcome,
    FileMoveReceipt, FileMoveReceiptOutcome, FileMoveResolution, FileParkOutcome, FileParkRequest,
    FileParkResolution, FilePromotionOutcome, FilePromotionReceipt, FilePromotionReceiptOutcome,
    FilePromotionResolution, FileRemovalOutcome, FileRemovalResolution, FileRestoreOutcome,
    FileRestoreResolution, FileRevision, LeafName, ParkedDirectory, ParkedFile, SealedStagedFile,
    StageDiscardOutcome, StageDiscardResolution, StagedFile,
};
use sha2::{Digest, Sha256, Sha512};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

const MANAGED_STORAGE_EFFECT_LOCK_INVARIANT: &str = "managed storage effect lock poisoned";

#[derive(Clone)]
pub struct ManagedInstanceEffectAuthority {
    inner: Arc<ManagedInstanceEffectState>,
}

pub(crate) struct WeakManagedInstanceEffectAuthority {
    inner: Weak<ManagedInstanceEffectState>,
}

struct ManagedInstanceEffectState {
    continuation: Mutex<Option<ManagedEffectContinuation>>,
    owner: EffectOwner,
}

enum ManagedEffectContinuation {
    FilePromotion(FilePromotionReceipt),
    FileMove(FileMoveReceipt),
    DirectoryMove(DirectoryMoveReceipt),
}

#[derive(Clone)]
pub(crate) struct ManagedStorageDirectory {
    directory: Directory,
    effects: ManagedInstanceEffectAuthority,
}

pub(crate) struct ManagedStorageFile {
    file: FileCapability,
    revision: FileRevision,
    effects: ManagedInstanceEffectAuthority,
}

#[cfg(test)]
pub(crate) struct TestManagedStorage {
    directory: ManagedStorageDirectory,
    _session: axial_fs::RootSession,
    _cleanup: TestStorageCleanup,
}

#[cfg(test)]
impl TestManagedStorage {
    pub(crate) fn new(path: &Path) -> Self {
        use axial_fs::{RootSession, RootSessionAcquireOutcome};

        let authority_root = path.with_extension("axial-performance-test-authority");
        let session = match RootSession::acquire(&authority_root) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            RootSessionAcquireOutcome::NoEffect(error) => {
                panic!("performance test authority acquisition had no effect: {error}")
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                match obligation.reconcile() {
                    RootSessionAcquireOutcome::Acquired(session) => session,
                    RootSessionAcquireOutcome::NoEffect(error) => {
                        panic!("performance test authority reconciliation had no effect: {error}")
                    }
                    RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                        let message = obligation.error().to_string();
                        match obligation.cleanup() {
                            Ok(()) => panic!(
                                "performance test authority was cleaned after indeterminate acquisition: {message}"
                            ),
                            Err(obligation) => {
                                std::mem::forget(obligation);
                                panic!(
                                    "performance test authority remains retained after indeterminate acquisition: {message}"
                                );
                            }
                        }
                    }
                }
            }
        };
        let directory = session
            .admit_absolute_directory(path)
            .expect("admit performance test directory");
        let effects = ManagedInstanceEffectAuthority::bind(&directory)
            .expect("bind performance test effect authority");
        Self {
            directory: ManagedStorageDirectory::bind_instance_root(directory, effects)
                .expect("bind performance test directory"),
            _session: session,
            _cleanup: TestStorageCleanup {
                authority_root,
                managed_root: path.to_path_buf(),
            },
        }
    }

    pub(crate) fn directory(&self) -> &ManagedStorageDirectory {
        &self.directory
    }
}

#[cfg(test)]
struct TestStorageCleanup {
    authority_root: PathBuf,
    managed_root: PathBuf,
}

#[cfg(test)]
impl Drop for TestStorageCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.authority_root);
        let _ = std::fs::remove_dir_all(&self.managed_root);
    }
}

impl ManagedInstanceEffectAuthority {
    pub(crate) fn downgrade(&self) -> WeakManagedInstanceEffectAuthority {
        WeakManagedInstanceEffectAuthority {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub(crate) fn bind(instance_root: &Directory) -> io::Result<Self> {
        let anchor = instance_root.identity()?;
        let owner = instance_root.create_effect_owner()?;
        if owner.anchor_identity() != anchor {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed storage effect owner has the wrong instance anchor",
            ));
        }
        Ok(Self {
            inner: Arc::new(ManagedInstanceEffectState {
                continuation: Mutex::new(None),
                owner,
            }),
        })
    }

    pub(crate) fn anchor_identity(&self) -> DirectoryIdentity {
        self.inner.owner.anchor_identity()
    }

    fn continuation(&self) -> MutexGuard<'_, Option<ManagedEffectContinuation>> {
        self.inner
            .continuation
            .lock()
            .unwrap_or_else(|poisoned| {
                let _ = MANAGED_STORAGE_EFFECT_LOCK_INVARIANT;
                poisoned.into_inner()
            })
    }

    fn shares_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn settle(&self) -> io::Result<()> {
        let first_error = self.inner.owner.settle().err();
        let cleanup_enqueued = self.claim_continuation()?;
        let second_error = if cleanup_enqueued {
            self.inner.owner.settle().err()
        } else {
            None
        };
        match self.require_settled() {
            Ok(()) => Ok(()),
            Err(pending) => Err(first_error.or(second_error).unwrap_or(pending)),
        }
    }

    pub fn has_pending(&self) -> bool {
        self.inner.owner.has_pending() || self.continuation().is_some()
    }

    pub fn require_settled(&self) -> io::Result<()> {
        if self.continuation().is_some() {
            return Err(pending_effect_error());
        }
        self.inner.owner.require_settled()
    }

    fn claim_continuation(&self) -> io::Result<bool> {
        let Some(continuation) = self.continuation().take() else {
            return Ok(false);
        };
        let pending = match continuation {
            ManagedEffectContinuation::FilePromotion(receipt) => match receipt.claim() {
                FilePromotionReceiptOutcome::Pending(receipt) => {
                    Some(ManagedEffectContinuation::FilePromotion(receipt))
                }
                FilePromotionReceiptOutcome::Applied(file) => {
                    drop(file);
                    None
                }
                FilePromotionReceiptOutcome::NoEffect(staged) => {
                    return Ok(self.retain_claimed_stage_discard(staged));
                }
            },
            ManagedEffectContinuation::FileMove(receipt) => match receipt.claim() {
                FileMoveReceiptOutcome::Pending(receipt) => {
                    Some(ManagedEffectContinuation::FileMove(receipt))
                }
                FileMoveReceiptOutcome::Applied(file) | FileMoveReceiptOutcome::NoEffect(file) => {
                    drop(file);
                    None
                }
            },
            ManagedEffectContinuation::DirectoryMove(receipt) => match receipt.claim() {
                DirectoryMoveReceiptOutcome::Pending(receipt) => {
                    Some(ManagedEffectContinuation::DirectoryMove(receipt))
                }
                DirectoryMoveReceiptOutcome::Applied(directory)
                | DirectoryMoveReceiptOutcome::NoEffect(directory) => {
                    drop(directory);
                    None
                }
            },
        };
        if let Some(pending) = pending {
            self.store_continuation(pending);
        }
        Ok(false)
    }

    fn retain_claimed_stage_discard(&self, staged: SealedStagedFile) -> bool {
        let obligation = match staged.discard() {
            StageDiscardOutcome::Discarded => return false,
            StageDiscardOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
                StageDiscardResolution::Discarded => return false,
                StageDiscardResolution::Indeterminate(obligation) => obligation,
            },
        };
        let _ = self.retain(obligation, EffectOwner::retain_stage_discard);
        true
    }

    fn store_continuation(&self, continuation: ManagedEffectContinuation) {
        let mut slot = self.continuation();
        if slot.is_some() {
            fail_stop_linear_carrier(continuation);
        }
        *slot = Some(continuation);
    }

    fn retain<T>(
        &self,
        carrier: T,
        retain: impl Fn(&EffectOwner, T) -> Result<(), EffectOwnerRetentionError<T>>,
    ) -> io::Error {
        self.retain_with(carrier, retain, |()| {})
    }

    fn retain_with<T, Receipt>(
        &self,
        carrier: T,
        retain: impl Fn(&EffectOwner, T) -> Result<Receipt, EffectOwnerRetentionError<T>>,
        store: impl Fn(Receipt),
    ) -> io::Error {
        match retain(&self.inner.owner, carrier) {
            Ok(receipt) => store(receipt),
            Err(failure) => {
                let (error, carrier) = failure.into_parts();
                if error.kind() != io::ErrorKind::WouldBlock {
                    fail_stop_linear_carrier(carrier);
                }
                // State serializes each instance and proves it settled before mutation.
                // One exceptional settlement may release bounded owner backpressure.
                let _ = self.settle();
                match retain(&self.inner.owner, carrier) {
                    Ok(receipt) => store(receipt),
                    Err(failure) => {
                        let (_, carrier) = failure.into_parts();
                        fail_stop_linear_carrier(carrier);
                    }
                }
            }
        }
        pending_effect_error()
    }

    fn retain_file_promotion(&self, obligation: axial_fs::FilePromotionObligation) -> io::Error {
        self.retain_with(
            obligation,
            EffectOwner::retain_file_promotion,
            |receipt| self.store_continuation(ManagedEffectContinuation::FilePromotion(receipt)),
        )
    }

    fn retain_file_move(&self, obligation: axial_fs::FileMoveObligation) -> io::Error {
        self.retain_with(obligation, EffectOwner::retain_file_move, |receipt| {
            self.store_continuation(ManagedEffectContinuation::FileMove(receipt));
        })
    }

    fn retain_directory_move(&self, obligation: axial_fs::DirectoryMoveObligation) -> io::Error {
        self.retain_with(obligation, EffectOwner::retain_directory_move, |receipt| {
            self.store_continuation(ManagedEffectContinuation::DirectoryMove(receipt));
        })
    }
}

impl WeakManagedInstanceEffectAuthority {
    pub(crate) fn upgrade(&self) -> Option<ManagedInstanceEffectAuthority> {
        self.inner
            .upgrade()
            .map(|inner| ManagedInstanceEffectAuthority { inner })
    }
}

impl ManagedStorageDirectory {
    pub(crate) fn bind_instance_root(
        directory: Directory,
        effects: ManagedInstanceEffectAuthority,
    ) -> io::Result<Self> {
        if directory.identity()? != effects.anchor_identity() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed storage root does not match its instance effect owner",
            ));
        }
        Ok(Self { directory, effects })
    }

    fn from_descendant(directory: Directory, effects: ManagedInstanceEffectAuthority) -> Self {
        Self { directory, effects }
    }

    pub(crate) fn directory(&self) -> &Directory {
        &self.directory
    }

    pub(crate) fn into_directory(self) -> Directory {
        self.directory
    }

    pub(crate) fn effect_owner(&self) -> &ManagedInstanceEffectAuthority {
        &self.effects
    }

    pub(crate) fn settle_pending_effects(&self) -> io::Result<()> {
        self.effects.settle()
    }

    pub(crate) fn open_child(&self, name: &str) -> io::Result<Option<Self>> {
        let leaf = managed_leaf(name)?;
        match self.directory.open_directory(&leaf) {
            Ok(directory) => Ok(Some(Self::from_descendant(directory, self.effects.clone()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open_observed_child(&self, entry: &DirectoryEntry) -> io::Result<Self> {
        if entry.kind() != EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed storage entry is not a directory",
            ));
        }
        self.directory
            .open_observed_directory(entry)
            .map(|directory| Self::from_descendant(directory, self.effects.clone()))
    }

    pub(crate) fn open_or_create_child(&self, name: &str) -> io::Result<Self> {
        if let Some(directory) = self.open_child(name)? {
            return Ok(directory);
        }
        let leaf = managed_leaf(name)?;
        let directory = match self.directory.create_directory(&leaf) {
            DirectoryCreateOutcome::Created(directory) => directory,
            DirectoryCreateOutcome::NoEffect(error)
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                return self.open_child(name)?.ok_or(error);
            }
            DirectoryCreateOutcome::NoEffect(error) => return Err(error),
            DirectoryCreateOutcome::CreatedUnclassified {
                error,
                preservation,
            } => {
                return match preservation.acknowledge_preserved() {
                    Ok(()) => Err(error),
                    Err(preservation) => Err(self.effects.retain(
                        preservation,
                        EffectOwner::retain_directory_create_preservation,
                    )),
                };
            }
            DirectoryCreateOutcome::AppliedUnverified(obligation) => {
                match obligation.reconcile() {
                    DirectoryCreateResolution::Created(directory) => directory,
                    DirectoryCreateResolution::Indeterminate(obligation) => {
                        return Err(self.effects.retain(
                            obligation,
                            EffectOwner::retain_directory_create_completion,
                        ));
                    }
                }
            }
        };
        Ok(Self::from_descendant(directory, self.effects.clone()))
    }

    pub(crate) fn open_relative_directory(&self, relative: &Path) -> io::Result<Self> {
        walk_relative(self, relative, false)
    }

    pub(crate) fn open_or_create_relative_directory(&self, relative: &Path) -> io::Result<Self> {
        walk_relative(self, relative, true)
    }

    pub(crate) fn open_file(&self, relative: &Path) -> io::Result<ManagedStorageFile> {
        let (parent, name) = self.resolve_file_parent(relative, false)?;
        let file = parent.directory.open_file(&name)?;
        ManagedStorageFile::new(file, parent.effects)
    }

    pub(crate) fn open_file_if_present(
        &self,
        relative: &Path,
    ) -> io::Result<Option<ManagedStorageFile>> {
        match self.open_file(relative) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn entries(&self, limit: usize) -> io::Result<DirectoryListing> {
        self.directory.entries(limit)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.directory.sync()
    }

    pub(crate) fn create_file_create_new(
        &self,
        relative: &Path,
        contents: &[u8],
    ) -> io::Result<ManagedStorageFile> {
        let (parent, name) = self.resolve_file_parent(relative, true)?;
        let mut staged = settle_file_create(parent.directory.create_stage(), &self.effects)?;
        if let Err(error) = staged.write_all(contents) {
            return discard_stage_after(staged, error, &self.effects);
        }
        let sealed = match staged.seal() {
            Ok(sealed) => sealed,
            Err(failure) => {
                let error = copy_io_error(failure.error());
                return discard_stage_after(failure.into_staged(), error, &self.effects);
            }
        };
        promote_stage(sealed, &parent, &name, &self.effects)
    }

    pub(crate) fn copy_file_create_new(
        &self,
        source: &ManagedStorageFile,
        relative: &Path,
        max_bytes: u64,
    ) -> io::Result<ManagedStorageFile> {
        if source.size() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed storage copy exceeds its byte budget",
            ));
        }
        if !self.effects.shares_authority(&source.effects) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage copy crossed its effect authority",
            ));
        }
        let (parent, name) = self.resolve_file_parent(relative, true)?;
        let mut staged = settle_file_create(parent.directory.create_stage(), &self.effects)?;
        let copied = (|| -> io::Result<()> {
            let mut reader = source.file.reader(max_bytes)?;
            let mut writer = staged.writer()?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                writer.write_all(&buffer[..read])?;
            }
            writer.finish()?;
            reader.finish()?;
            source.validate()
        })();
        if let Err(error) = copied {
            return discard_stage_after(staged, error, &self.effects);
        }
        let sealed = match staged.seal() {
            Ok(sealed) => sealed,
            Err(failure) => {
                let error = copy_io_error(failure.error());
                return discard_stage_after(failure.into_staged(), error, &self.effects);
            }
        };
        promote_stage(sealed, &parent, &name, &self.effects)
    }

    pub(crate) fn move_file_no_replace(
        &self,
        source: ManagedStorageFile,
        destination_relative: &Path,
    ) -> io::Result<ManagedStorageFile> {
        if !self.effects.shares_authority(&source.effects) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage move crossed its effect authority",
            ));
        }
        let (destination, name) = self.resolve_file_parent(destination_relative, true)?;
        settle_file_move(
            source.file.move_no_replace(&destination.directory, &name),
            &self.effects,
        )
        .and_then(|file| ManagedStorageFile::new(file, self.effects.clone()))
    }

    pub(crate) fn move_child_directory_no_replace(
        &self,
        source: ManagedStorageDirectory,
        destination_parent: &ManagedStorageDirectory,
        destination_name: &str,
    ) -> io::Result<ManagedStorageDirectory> {
        if !self.effects.shares_authority(&source.effects)
            || !self.effects.shares_authority(&destination_parent.effects)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage directory move crossed its effect authority",
            ));
        }
        let name = managed_leaf(destination_name)?;
        settle_directory_move(
            source.into_directory().move_no_replace(
                destination_parent.directory(),
                &name,
            ),
            &self.effects,
        )
        .map(|directory| Self::from_descendant(directory, self.effects.clone()))
    }

    pub(crate) fn park_file_as(
        &self,
        file: ManagedStorageFile,
        park_name: &str,
        sha256: [u8; 32],
    ) -> io::Result<ParkedFile> {
        if !self.effects.shares_authority(&file.effects) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage park crossed its effect authority",
            ));
        }
        let park_name = managed_leaf(park_name)?;
        let request = file.into_park_request(sha256);
        settle_file_park(
            self.directory.park_file_as(request, park_name),
            &self.effects,
            EffectOwner::retain_file_park_removal,
        )
    }

    pub(crate) fn park_file_for_restore_as(
        &self,
        file: ManagedStorageFile,
        park_name: &str,
        sha256: [u8; 32],
    ) -> io::Result<ParkedFile> {
        if !self.effects.shares_authority(&file.effects) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage park crossed its effect authority",
            ));
        }
        let park_name = managed_leaf(park_name)?;
        let request = file.into_park_request(sha256);
        settle_file_park(
            self.directory.park_file_as(request, park_name),
            &self.effects,
            EffectOwner::retain_file_park_restore,
        )
    }

    pub(crate) fn admit_existing_file_park(
        &self,
        original_name: &str,
        parked_name: &str,
        expected_sha256: [u8; 32],
    ) -> io::Result<ParkedFile> {
        let original_name = managed_leaf(original_name)?;
        let parked = self.open_file(Path::new(parked_name))?;
        self.directory.admit_existing_file_park(
            &original_name,
            parked.into_park_request(expected_sha256),
        )
    }

    pub(crate) fn park_child_directory_as(
        &self,
        child: ManagedStorageDirectory,
        park_name: &str,
    ) -> io::Result<ParkedDirectory> {
        if !self.effects.shares_authority(&child.effects) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage directory park crossed its effect authority",
            ));
        }
        settle_directory_park(
            child.into_directory().park_as(managed_leaf(park_name)?),
            &self.effects,
        )
    }

    pub(crate) fn admit_existing_directory_park(
        &self,
        original_name: &str,
        parked_name: &str,
    ) -> io::Result<ParkedDirectory> {
        let original_name = managed_leaf(original_name)?;
        let parked = self
            .open_child(parked_name)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "parked directory is missing"))?
            .into_directory();
        let revision = parked.revision()?;
        self.directory
            .admit_existing_directory_park(&original_name, parked, &revision)
    }

    fn resolve_file_parent(
        &self,
        relative: &Path,
        create_parent: bool,
    ) -> io::Result<(Self, LeafName)> {
        if relative.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage file name must be relative",
            ));
        }
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage file has no direct name",
            )
        })?;
        let parent = if create_parent {
            self.open_or_create_relative_directory(parent)?
        } else {
            self.open_relative_directory(parent)?
        };
        Ok((parent, managed_leaf_os(name)?))
    }
}

impl ManagedStorageFile {
    fn new(file: FileCapability, effects: ManagedInstanceEffectAuthority) -> io::Result<Self> {
        let revision = file.revision()?;
        file.validate_revision(&revision)?;
        Ok(Self {
            file,
            revision,
            effects,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.revision.size()
    }

    pub(crate) fn into_parts(self) -> (FileCapability, FileRevision) {
        (self.file, self.revision)
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        self.file.validate_revision(&self.revision)
    }

    pub(crate) fn sha512(&self, max_bytes: u64) -> io::Result<String> {
        self.sha512_bytes(max_bytes).map(hex::encode)
    }

    pub(crate) fn sha512_bytes(&self, max_bytes: u64) -> io::Result<Vec<u8>> {
        let mut reader = self.file.reader(max_bytes)?;
        let mut hasher = Sha512::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        reader.finish()?;
        self.validate()?;
        Ok(hasher.finalize().to_vec())
    }

    pub(crate) fn digests(&self, max_bytes: u64) -> io::Result<([u8; 32], Vec<u8>)> {
        let mut reader = self.file.reader(max_bytes)?;
        let mut sha256 = Sha256::new();
        let mut sha512 = Sha512::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            sha256.update(&buffer[..read]);
            sha512.update(&buffer[..read]);
        }
        reader.finish()?;
        self.validate()?;
        Ok((sha256.finalize().into(), sha512.finalize().to_vec()))
    }

    pub(crate) fn read_bounded(&self, max_bytes: u64) -> io::Result<Vec<u8>> {
        let bytes = self.file.read_bounded(max_bytes)?;
        self.validate()?;
        Ok(bytes)
    }

    fn into_park_request(self, sha256: [u8; 32]) -> FileParkRequest {
        let (file, revision) = self.into_parts();
        file.park_request(ExpectedFileContent::new(revision, sha256))
    }
}

pub(crate) fn settle_parked_file_removal(
    owner: &ManagedStorageDirectory,
    parked: ParkedFile,
) -> io::Result<()> {
    match parked.remove() {
        FileRemovalOutcome::Removed => Ok(()),
        FileRemovalOutcome::NoEffect { error: _, parked } => Err(owner.effects.retain(
            parked,
            EffectOwner::retain_parked_file_removal,
        )),
        FileRemovalOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileRemovalResolution::Removed => Ok(()),
            FileRemovalResolution::NoEffect(parked) => Err(owner.effects.retain(
                parked,
                EffectOwner::retain_parked_file_removal,
            )),
            FileRemovalResolution::Indeterminate(obligation) => Err(owner.effects.retain(
                obligation,
                EffectOwner::retain_file_removal,
            )),
        },
    }
}

pub(crate) fn settle_parked_file_restore(
    owner: &ManagedStorageDirectory,
    parked: ParkedFile,
) -> io::Result<ManagedStorageFile> {
    let file = match parked.restore() {
        FileRestoreOutcome::Restored(file) => file,
        FileRestoreOutcome::NoEffect { error: _, parked } => {
            return Err(owner.effects.retain(
                parked,
                EffectOwner::retain_parked_file_restore,
            ));
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileRestoreResolution::Restored(file) => file,
            FileRestoreResolution::NoEffect(parked) => {
                return Err(owner.effects.retain(
                    parked,
                    EffectOwner::retain_parked_file_restore,
                ));
            }
            FileRestoreResolution::Indeterminate(obligation) => {
                return Err(owner.effects.retain(
                    obligation,
                    EffectOwner::retain_file_restore,
                ));
            }
        },
    };
    ManagedStorageFile::new(file, owner.effects.clone())
}

pub(crate) fn retain_parked_file_after(
    owner: &ManagedStorageDirectory,
    parked: ParkedFile,
) -> io::Error {
    owner
        .effects
        .retain(parked, EffectOwner::retain_parked_file_preservation)
}

pub(crate) fn settle_parked_directory_removal(
    owner: &ManagedStorageDirectory,
    parked: ParkedDirectory,
) -> io::Result<()> {
    match parked.remove_empty() {
        DirectoryRemovalOutcome::Removed => Ok(()),
        DirectoryRemovalOutcome::NoEffect { error: _, parked } => Err(owner.effects.retain(
            parked,
            EffectOwner::retain_parked_directory_removal,
        )),
        DirectoryRemovalOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            DirectoryRemovalResolution::Removed => Ok(()),
            DirectoryRemovalResolution::NoEffect(parked) => Err(owner.effects.retain(
                parked,
                EffectOwner::retain_parked_directory_removal,
            )),
            DirectoryRemovalResolution::Indeterminate(obligation) => Err(owner.effects.retain(
                obligation,
                EffectOwner::retain_directory_removal,
            )),
        },
    }
}

fn walk_relative(
    root: &ManagedStorageDirectory,
    relative: &Path,
    create: bool,
) -> io::Result<ManagedStorageDirectory> {
    if relative.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed storage directory name must be relative",
        ));
    }
    let mut current = root.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage directory is not a normalized descendant",
            ));
        };
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed storage directory name is not UTF-8",
            )
        })?;
        current = if create {
            current.open_or_create_child(name)?
        } else {
            current.open_child(name)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "managed storage directory is missing")
            })?
        };
    }
    Ok(current)
}

fn settle_file_create(
    outcome: FileCreateOutcome,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<StagedFile> {
    match outcome {
        FileCreateOutcome::Created(staged) => Ok(staged),
        FileCreateOutcome::NoEffect(error) => Err(error),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(staged) => Ok(staged),
            FileCreateResolution::Indeterminate(obligation) => Err(effects.retain(
                obligation,
                EffectOwner::retain_stage_create_cleanup,
            )),
        },
    }
}

fn promote_stage(
    sealed: SealedStagedFile,
    destination: &ManagedStorageDirectory,
    name: &LeafName,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<ManagedStorageFile> {
    match sealed.promote_no_replace(
        destination.directory(),
        destination.directory(),
        name,
    ) {
        FilePromotionOutcome::Applied(file) => ManagedStorageFile::new(file, effects.clone()),
        FilePromotionOutcome::NoEffect { error, staged } => {
            discard_sealed_after(staged, error, effects)
        }
        FilePromotionOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FilePromotionResolution::Applied(file) => {
                ManagedStorageFile::new(file, effects.clone())
            }
            FilePromotionResolution::NoEffect(staged) => discard_sealed_after(
                staged,
                io::Error::other("managed storage promotion had no effect"),
                effects,
            ),
            FilePromotionResolution::Indeterminate(obligation) => {
                Err(effects.retain_file_promotion(obligation))
            }
        },
    }
}

fn settle_file_move(
    outcome: FileMoveOutcome,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<FileCapability> {
    match outcome {
        FileMoveOutcome::Applied(file) => Ok(file),
        FileMoveOutcome::NoEffect { error, file: _ } => Err(error),
        FileMoveOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileMoveResolution::Applied(file) => Ok(file),
            FileMoveResolution::NoEffect(_) => {
                Err(io::Error::other("managed storage file move had no effect"))
            }
            FileMoveResolution::Indeterminate(obligation) => {
                Err(effects.retain_file_move(obligation))
            }
        },
    }
}

fn settle_directory_move(
    outcome: DirectoryMoveOutcome,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<Directory> {
    match outcome {
        DirectoryMoveOutcome::Applied(directory) => Ok(directory),
        DirectoryMoveOutcome::NoEffect {
            error,
            directory: _,
        } => Err(error),
        DirectoryMoveOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            DirectoryMoveResolution::Applied(directory) => Ok(directory),
            DirectoryMoveResolution::NoEffect(_) => Err(io::Error::other(
                "managed storage directory move had no effect",
            )),
            DirectoryMoveResolution::Indeterminate(obligation) => {
                Err(effects.retain_directory_move(obligation))
            }
        },
    }
}

fn settle_file_park(
    outcome: FileParkOutcome,
    effects: &ManagedInstanceEffectAuthority,
    retain: fn(
        &EffectOwner,
        axial_fs::FileParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<axial_fs::FileParkObligation>>,
) -> io::Result<ParkedFile> {
    match outcome {
        FileParkOutcome::Parked(parked) => Ok(parked),
        FileParkOutcome::NoEffect { error, request: _ } => Err(error),
        FileParkOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileParkResolution::Parked(parked) => Ok(parked),
            FileParkResolution::NoEffect(_) => {
                Err(io::Error::other("managed storage file park had no effect"))
            }
            FileParkResolution::Indeterminate(obligation) => {
                Err(effects.retain(obligation, retain))
            }
        },
    }
}

fn settle_directory_park(
    outcome: DirectoryParkOutcome,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<ParkedDirectory> {
    match outcome {
        DirectoryParkOutcome::Parked(parked) => Ok(parked),
        DirectoryParkOutcome::NoEffect {
            error,
            directory: _,
        } => Err(error),
        DirectoryParkOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            DirectoryParkResolution::Parked(parked) => Ok(parked),
            DirectoryParkResolution::NoEffect(_) => Err(io::Error::other(
                "managed storage directory park had no effect",
            )),
            DirectoryParkResolution::Indeterminate(obligation) => Err(effects.retain(
                obligation,
                EffectOwner::retain_directory_park_removal,
            )),
        },
    }
}

fn discard_stage_after<T>(
    staged: StagedFile,
    original: io::Error,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<T> {
    settle_stage_discard(staged.discard(), effects).and(Err(original))
}

fn discard_sealed_after<T>(
    staged: SealedStagedFile,
    original: io::Error,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<T> {
    settle_stage_discard(staged.discard(), effects).and(Err(original))
}

fn settle_stage_discard(
    outcome: StageDiscardOutcome,
    effects: &ManagedInstanceEffectAuthority,
) -> io::Result<()> {
    match outcome {
        StageDiscardOutcome::Discarded => Ok(()),
        StageDiscardOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            StageDiscardResolution::Discarded => Ok(()),
            StageDiscardResolution::Indeterminate(obligation) => {
                Err(effects.retain(obligation, EffectOwner::retain_stage_discard))
            }
        },
    }
}

fn managed_leaf(name: &str) -> io::Result<LeafName> {
    managed_leaf_os(OsStr::new(name))
}

fn managed_leaf_os(name: &OsStr) -> io::Result<LeafName> {
    LeafName::new(name.to_os_string()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed storage name is not a direct native leaf",
        )
    })
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

fn pending_effect_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "managed storage effect remains retained and indeterminate",
    )
}

fn fail_stop_linear_carrier<T>(_carrier: T) -> ! {
    std::process::abort()
}
