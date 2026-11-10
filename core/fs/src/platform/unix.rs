use super::*;
use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::{Component, PathBuf};
use std::sync::{Arc, RwLock};
#[cfg(test)]
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

pub(crate) type DirectoryHandle = OwnedFd;

pub(crate) struct FileCleanupHandle(File);

pub(crate) struct DirectoryCleanupHandle(DirectoryHandle);

pub(crate) struct RootGuard {
    handle: DirectoryHandle,
    identity: Identity,
    bindings: Vec<RootBinding>,
}

struct RootBinding {
    parent: DirectoryHandle,
    name: OsString,
    identity: Identity,
    exact_name: bool,
    exact_revision: Arc<RwLock<Option<DirectoryStamp>>>,
}

pub(crate) struct ProcessImageAncestry {
    image: OwnedFd,
    identity: Identity,
    bindings: Vec<ProcessImageBinding>,
}

struct ProcessImageBinding {
    parent: DirectoryHandle,
    name: OsString,
    identity: Identity,
    kind: EntryKind,
}

pub(crate) struct AbsoluteDirectoryGuard {
    handle: DirectoryHandle,
    identity: Identity,
    bindings: Vec<AbsoluteDirectoryBinding>,
    ancestry_complete: bool,
}

struct AbsoluteDirectoryBinding {
    parent: DirectoryHandle,
    name: OsString,
    identity: Identity,
    exact_name: bool,
    exact_revision: Arc<RwLock<Option<DirectoryStamp>>>,
}

pub(crate) struct RootConstruction {
    target: std::path::PathBuf,
    guard: Option<RootGuard>,
    created: Vec<RootCreatedBinding>,
    unclassified: Vec<RootCreationReservation>,
}

pub(crate) struct RootCreatedBinding {
    parent: DirectoryHandle,
    name: OsString,
    identity: Identity,
    child: Option<DirectoryHandle>,
    published: bool,
}

pub(crate) struct RootConstructionError {
    error: io::Error,
    construction: Option<RootConstruction>,
}

struct RootCreationReservation {
    parent: DirectoryHandle,
    name: OsString,
    child: Option<DirectoryHandle>,
    published: bool,
}

enum RootDirectoryCreationError {
    NoEffect(io::Error),
    Unclassified {
        error: io::Error,
        creation: RootCreationReservation,
    },
    Applied {
        error: io::Error,
        binding: RootCreatedBinding,
    },
}

pub(crate) enum LeaseAcquisitionOutcome {
    Acquired(LeaseHandle),
    NoEffect(io::Error),
    AppliedUnverified(LeaseAcquisitionObligation),
}

pub(crate) struct LeaseAcquisitionObligation {
    error: io::Error,
    handle: Option<File>,
    identity: Option<Identity>,
    name: OsString,
    state: LeaseAcquisitionState,
}

enum LeaseAcquisitionState {
    Created,
    Removed,
}

pub(crate) struct LeaseHandle {
    handle: File,
    process: u32,
    identity: Identity,
    root: RootGuard,
    name: OsString,
    name_class_revision: RwLock<Option<DirectoryStamp>>,
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        // A pre-exec duplicate must not retain this owner or unlock its parent.
        if self.process == std::process::id() {
            let _ = unsafe { libc::flock(self.handle.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Identity {
    device: rfs::Dev,
    inode: u64,
}

pub(crate) fn identity_witness(identity: Identity) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"axial-fs-unix-identity-v1");
    digest.update((identity.device as u64).to_le_bytes());
    digest.update(identity.inode.to_le_bytes());
    digest.finalize().into()
}

#[cfg(target_os = "linux")]
pub(crate) enum CreateTransientFileError {
    NoEffect(io::Error),
}

#[cfg(target_os = "linux")]
pub(crate) enum DiscardTransientFileError {
    Retained {
        error: io::Error,
        file: TransientFile,
    },
}

#[cfg(target_os = "linux")]
pub(crate) struct TransientFile {
    file: File,
    proc_path: PathBuf,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct FileStamp {
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct DirectoryStamp {
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

pub(crate) fn leaf_names_equal_native(_first: &OsStr, _second: &OsStr) -> bool {
    false
}

pub(crate) fn leaf_name_native_key(name: &OsStr) -> Vec<u8> {
    let mut key = vec![b'n'];
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn fill_leaf_name_native_key(name: &OsStr, key: &mut Vec<u8>) -> io::Result<()> {
    extend_preallocated_key(key, name.as_bytes())
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn process_image_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

pub(crate) const MAX_TREE_CLEAR_DEPTH: usize = 128;
const MAX_TREE_CLEAR_ENTRIES: usize = 1_000_000;
const EXACT_DIRECTORY_BINDING_VALIDATION_ATTEMPTS: usize = 4;
const LEASE_NAME_CLASS_VALIDATION_ATTEMPTS: usize = 4;

#[cfg(test)]
type ExactDirectoryBindingTestHook = Box<dyn FnMut(&OsStr)>;

#[cfg(test)]
thread_local! {
    static EXACT_DIRECTORY_BINDING_TEST_HOOK: RefCell<Option<ExactDirectoryBindingTestHook>> =
        RefCell::new(None);
}

#[cfg(test)]
pub(crate) struct ExactDirectoryBindingTestHookGuard {
    thread: std::thread::ThreadId,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

#[cfg(test)]
impl Drop for ExactDirectoryBindingTestHookGuard {
    fn drop(&mut self) {
        assert_eq!(
            self.thread,
            std::thread::current().id(),
            "exact-binding test hook guard changed threads"
        );
        EXACT_DIRECTORY_BINDING_TEST_HOOK.with(|slot| {
            slot.replace(None);
        });
    }
}

#[cfg(test)]
pub(crate) fn install_exact_directory_binding_test_hook(
    hook: ExactDirectoryBindingTestHook,
) -> ExactDirectoryBindingTestHookGuard {
    EXACT_DIRECTORY_BINDING_TEST_HOOK.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "exact-binding test hook is already installed"
        );
        slot.replace(Some(hook));
    });
    ExactDirectoryBindingTestHookGuard {
        thread: std::thread::current().id(),
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(test)]
fn run_exact_directory_binding_test_hook(name: &OsStr) {
    EXACT_DIRECTORY_BINDING_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(name);
        }
    });
}

#[cfg(test)]
thread_local! {
    static PUBLICATION_DIRECTORY_SYNC_TEST_OUTCOMES:
        RefCell<Option<VecDeque<Result<(), io::ErrorKind>>>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct PublicationDirectorySyncTestGuard {
    thread: std::thread::ThreadId,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

#[cfg(test)]
impl Drop for PublicationDirectorySyncTestGuard {
    fn drop(&mut self) {
        assert_eq!(
            self.thread,
            std::thread::current().id(),
            "publication-directory-sync test guard changed threads"
        );
        PUBLICATION_DIRECTORY_SYNC_TEST_OUTCOMES.with(|slot| {
            slot.replace(None);
        });
    }
}

#[cfg(test)]
pub(crate) fn install_publication_directory_sync_test_outcomes(
    outcomes: impl IntoIterator<Item = Result<(), io::ErrorKind>>,
) -> PublicationDirectorySyncTestGuard {
    PUBLICATION_DIRECTORY_SYNC_TEST_OUTCOMES.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "publication-directory-sync test outcomes are already installed"
        );
        slot.replace(Some(outcomes.into_iter().collect()));
    });
    PublicationDirectorySyncTestGuard {
        thread: std::thread::current().id(),
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(test)]
fn publication_directory_sync_test_outcome() -> Option<Result<(), io::ErrorKind>> {
    PUBLICATION_DIRECTORY_SYNC_TEST_OUTCOMES
        .with(|slot| slot.borrow_mut().as_mut().and_then(VecDeque::pop_front))
}

fn identity_from_stat(stat: rfs::Stat) -> Identity {
    Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

pub(crate) fn capture_process_image_ancestry(path: &Path) -> io::Result<ProcessImageAncestry> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "current executable path is not absolute",
        ));
    }
    let names = absolute_normal_components(path)?;
    let (image_name, directory_names) = names
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "executable has no leaf"))?;
    let mut current = rfs::open("/", directory_flags(), Mode::empty())?;
    let mut bindings = Vec::new();
    for name in directory_names {
        let child = rfs::openat(&current, name, directory_flags(), Mode::empty())?;
        let identity = directory_identity(&child)?;
        bindings.push(ProcessImageBinding {
            parent: current,
            name: name.clone(),
            identity,
            kind: EntryKind::Directory,
        });
        current = child;
    }
    let image = rfs::openat(&current, image_name, process_image_flags(), Mode::empty())?;
    let identity = process_image_identity(&image)?;
    bindings.push(ProcessImageBinding {
        parent: current,
        name: image_name.clone(),
        identity,
        kind: EntryKind::File,
    });
    let ancestry = ProcessImageAncestry {
        image,
        identity,
        bindings,
    };
    validate_process_image_ancestry(&ancestry)?;
    Ok(ancestry)
}

pub(crate) fn validate_process_image_outside_root(
    ancestry: &ProcessImageAncestry,
    root: &RootGuard,
) -> io::Result<()> {
    validate_root(root)?;
    validate_process_image_ancestry(ancestry)?;
    for binding in &ancestry.bindings {
        if binding.kind == EntryKind::Directory && binding.identity == root.identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "process image is inside the application root",
            ));
        }
    }
    Ok(())
}

fn validate_process_image_ancestry(ancestry: &ProcessImageAncestry) -> io::Result<()> {
    if process_image_identity(&ancestry.image)? != ancestry.identity {
        return Err(binding_changed("process image changed identity"));
    }
    for binding in &ancestry.bindings {
        let expected_kind = binding.kind;
        match entry_observation(&binding.parent, &binding.name)? {
            Some((kind, identity)) if kind == expected_kind && identity == binding.identity => {}
            _ => return Err(binding_changed("process image ancestry changed binding")),
        }
    }
    Ok(())
}

fn process_image_identity(image: &OwnedFd) -> io::Result<Identity> {
    let stat = rfs::fstat(image)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process image is not a regular file",
        ));
    }
    Ok(identity_from_stat(stat))
}

pub(crate) fn open_absolute_directory_guard(path: &Path) -> io::Result<AbsoluteDirectoryGuard> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "external directory is not absolute",
        ));
    }
    let names = absolute_normal_components(path)?;
    let mut current = rfs::open("/", directory_flags(), Mode::empty())?;
    let mut bindings = Vec::new();
    for name in names {
        let child = rfs::openat(&current, &name, directory_flags(), Mode::empty())?;
        let identity = directory_identity(&child)?;
        bindings.push(AbsoluteDirectoryBinding {
            parent: current,
            name,
            identity,
            exact_name: true,
            exact_revision: Arc::new(RwLock::new(None)),
        });
        current = child;
    }
    let identity = directory_identity(&current)?;
    let guard = AbsoluteDirectoryGuard {
        handle: current,
        identity,
        bindings,
        ancestry_complete: true,
    };
    validate_absolute_directory_guard(&guard)?;
    Ok(guard)
}

pub(crate) fn absolute_directory_handle(guard: &AbsoluteDirectoryGuard) -> &DirectoryHandle {
    &guard.handle
}

pub(crate) fn root_construction_from_absolute_directory_guard(
    guard: &AbsoluteDirectoryGuard,
) -> io::Result<RootConstruction> {
    validate_absolute_directory_guard(guard)?;
    let handle = clone_directory_handle(&guard.handle)?;
    let root = RootGuard {
        handle,
        identity: guard.identity,
        bindings: Vec::new(),
    };
    validate_root(&root)?;
    Ok(RootConstruction {
        target: PathBuf::new(),
        guard: Some(root),
        created: Vec::new(),
        unclassified: Vec::new(),
    })
}

pub(crate) fn absolute_directory_identity(guard: &AbsoluteDirectoryGuard) -> Identity {
    guard.identity
}

pub(crate) fn absolute_directory_anchor_is_ancestor(
    guard: &AbsoluteDirectoryGuard,
    ancestor: Identity,
) -> io::Result<bool> {
    let identity = match guard.bindings.first() {
        Some(binding) => directory_identity(&binding.parent)?,
        None => guard.identity,
    };
    if identity == ancestor {
        return Ok(true);
    }
    if !guard.ancestry_complete {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory capability has incomplete retained ancestry",
        ));
    }
    Ok(false)
}

pub(crate) fn root_has_physical_ancestor(root: &RootGuard, ancestor: Identity) -> io::Result<bool> {
    let first = root.bindings.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "root capability has no retained ancestry",
        )
    })?;
    Ok(root.identity == ancestor
        || root
            .bindings
            .iter()
            .any(|binding| binding.identity == ancestor)
        || directory_identity(&first.parent)? == ancestor)
}

pub(crate) fn absolute_directory_has_ancestor(
    guard: &AbsoluteDirectoryGuard,
    ancestor: Identity,
) -> bool {
    guard.identity == ancestor
        || guard
            .bindings
            .iter()
            .any(|binding| binding.identity == ancestor)
}

pub(crate) fn validate_absolute_directory_guard(
    guard: &AbsoluteDirectoryGuard,
) -> io::Result<Identity> {
    let identity = directory_identity(&guard.handle)?;
    if identity != guard.identity {
        return Err(binding_changed("external directory changed identity"));
    }
    for binding in &guard.bindings {
        let state = if binding.exact_name {
            exact_directory_binding_state(
                &binding.parent,
                &binding.name,
                binding.identity,
                &binding.exact_revision,
            )?
        } else {
            directory_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(binding_changed(
                "external directory ancestry changed binding",
            ));
        }
    }
    Ok(identity)
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_absolute_directory_guard_preallocated(
    guard: &AbsoluteDirectoryGuard,
    buffer: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<Identity> {
    let identity = directory_identity_preallocated(&guard.handle)?;
    if identity != guard.identity {
        return Err(io::ErrorKind::InvalidData.into());
    }
    for binding in &guard.bindings {
        let state = if binding.exact_name {
            exact_directory_binding_state_preallocated(
                &binding.parent,
                &binding.name,
                binding.identity,
                buffer,
            )?
        } else {
            directory_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    Ok(identity)
}

pub(crate) fn absolute_directory_is_outside_root(
    guard: &AbsoluteDirectoryGuard,
    root: &RootGuard,
) -> io::Result<bool> {
    validate_root(root)?;
    validate_absolute_directory_guard(guard)?;
    if guard.identity == root.identity {
        return Ok(false);
    }
    if guard
        .bindings
        .iter()
        .any(|binding| binding.identity == root.identity)
    {
        return Ok(false);
    }
    Ok(true)
}

fn absolute_normal_components(path: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => names.push(name.to_os_string()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "absolute path contains an unsupported component",
                ));
            }
        }
    }
    Ok(names)
}

pub(crate) fn open_or_create_root(path: &Path) -> Result<RootConstruction, RootConstructionError> {
    if !path.is_absolute() {
        return Err(RootConstructionError::without_effect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application root is not absolute",
        )));
    }
    let mut current = rfs::open("/", directory_flags(), Mode::empty())
        .map_err(|error| RootConstructionError::without_effect(error.into()))?;
    let mut bindings = Vec::new();
    let mut created = Vec::new();
    let mut unclassified = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(root_construction_error(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "application root contains an unsupported component",
                ),
                path,
                created,
                unclassified,
            ));
        };
        let child = match rfs::openat(&current, name, directory_flags(), Mode::empty()) {
            Ok(child) => child,
            Err(error) if error == rustix::io::Errno::NOENT => {
                if created.try_reserve(1).is_err() || unclassified.try_reserve(1).is_err() {
                    return Err(root_construction_error(
                        io::Error::other("could not reserve root construction recovery capacity"),
                        path,
                        created,
                        unclassified,
                    ));
                }
                match create_and_publish_root_directory(&current, name) {
                    Ok((child, binding)) => {
                        created.push(binding);
                        child
                    }
                    Err(RootDirectoryCreationError::NoEffect(error)) => {
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                    Err(RootDirectoryCreationError::Applied { error, binding }) => {
                        created.push(binding);
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                    Err(RootDirectoryCreationError::Unclassified { error, creation }) => {
                        unclassified.push(creation);
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                }
            }
            Err(error) => {
                return Err(root_construction_error(
                    error.into(),
                    path,
                    created,
                    unclassified,
                ));
            }
        };
        let identity = match directory_identity(&child) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(root_construction_error(error, path, created, unclassified));
            }
        };
        bindings.push(RootBinding {
            parent: current,
            name: name.to_os_string(),
            identity,
            exact_name: false,
            exact_revision: Arc::new(RwLock::new(None)),
        });
        current = child;
    }
    let identity = match directory_identity(&current) {
        Ok(identity) => identity,
        Err(error) => {
            return Err(root_construction_error(error, path, created, unclassified));
        }
    };
    if bindings.is_empty() {
        return Err(root_construction_error(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "application root cannot be the filesystem root",
            ),
            path,
            created,
            unclassified,
        ));
    }
    let guard = RootGuard {
        handle: current,
        identity,
        bindings,
    };
    if let Err(error) = validate_root(&guard) {
        return Err(root_construction_error_with_guard(
            error,
            path,
            guard,
            created,
            unclassified,
        ));
    }
    Ok(RootConstruction {
        target: path.to_path_buf(),
        guard: Some(guard),
        created,
        unclassified,
    })
}

fn create_and_publish_root_directory(
    parent: &DirectoryHandle,
    target_name: &OsStr,
) -> Result<(DirectoryHandle, RootCreatedBinding), RootDirectoryCreationError> {
    use rand::RngCore as _;

    for _ in 0..32 {
        let mut nonce = [0_u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let name = OsString::from(format!(".axial-root-create-{}", hex::encode(nonce)));
        let mut reservation = RootCreationReservation {
            parent: clone_directory_handle(parent).map_err(RootDirectoryCreationError::NoEffect)?,
            name,
            child: None,
            published: false,
        };
        match rfs::mkdirat(parent, &reservation.name, Mode::from_bits_truncate(0o700)) {
            Ok(()) => {
                let child = match rfs::openat(
                    parent,
                    &reservation.name,
                    directory_flags(),
                    Mode::empty(),
                ) {
                    Ok(child) => child,
                    Err(error) => {
                        return Err(RootDirectoryCreationError::Unclassified {
                            error: error.into(),
                            creation: reservation,
                        });
                    }
                };
                let identity = match directory_identity(&child) {
                    Ok(identity) => identity,
                    Err(error) => {
                        reservation.child = Some(child);
                        return Err(RootDirectoryCreationError::Unclassified {
                            error,
                            creation: reservation,
                        });
                    }
                };
                let retained_child = match clone_directory_handle(&child) {
                    Ok(retained) => retained,
                    Err(error) => {
                        let RootCreationReservation {
                            parent,
                            name,
                            published,
                            ..
                        } = reservation;
                        return Err(RootDirectoryCreationError::Applied {
                            error,
                            binding: RootCreatedBinding {
                                parent,
                                name,
                                identity,
                                child: Some(child),
                                published,
                            },
                        });
                    }
                };
                let RootCreationReservation {
                    parent,
                    name,
                    published,
                    ..
                } = reservation;
                let mut binding = RootCreatedBinding {
                    parent,
                    name: name.clone(),
                    identity,
                    child: Some(retained_child),
                    published,
                };
                if let Err(error) = rfs::renameat_with(
                    &binding.parent,
                    &name,
                    &binding.parent,
                    target_name,
                    rfs::RenameFlags::NOREPLACE,
                ) {
                    return Err(RootDirectoryCreationError::Applied {
                        error: error.into(),
                        binding,
                    });
                }
                binding.name = target_name.to_os_string();
                binding.published = true;
                return Ok((child, binding));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => return Err(RootDirectoryCreationError::NoEffect(error.into())),
        }
    }
    Err(RootDirectoryCreationError::NoEffect(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a temporary root directory",
    )))
}

fn clone_directory_handle(directory: &DirectoryHandle) -> io::Result<DirectoryHandle> {
    Ok(rfs::openat(
        directory,
        ".",
        directory_flags(),
        Mode::empty(),
    )?)
}

impl RootConstructionError {
    fn without_effect(error: io::Error) -> Self {
        Self {
            error,
            construction: None,
        }
    }

    pub(crate) fn into_parts(self) -> (io::Error, Option<RootConstruction>) {
        (self.error, self.construction)
    }
}

fn root_construction_error(
    error: io::Error,
    target: &Path,
    created: Vec<RootCreatedBinding>,
    unclassified: Vec<RootCreationReservation>,
) -> RootConstructionError {
    let construction =
        (!created.is_empty() || !unclassified.is_empty()).then(|| RootConstruction {
            target: target.to_path_buf(),
            guard: None,
            created,
            unclassified,
        });
    RootConstructionError {
        error,
        construction,
    }
}

fn root_construction_error_with_guard(
    error: io::Error,
    target: &Path,
    guard: RootGuard,
    created: Vec<RootCreatedBinding>,
    unclassified: Vec<RootCreationReservation>,
) -> RootConstructionError {
    if created.is_empty() && unclassified.is_empty() {
        return RootConstructionError::without_effect(error);
    }
    RootConstructionError {
        error,
        construction: Some(RootConstruction {
            target: target.to_path_buf(),
            guard: Some(guard),
            created,
            unclassified,
        }),
    }
}

pub(crate) fn root_construction_has_effect(construction: &RootConstruction) -> bool {
    !construction.created.is_empty() || !construction.unclassified.is_empty()
}

pub(crate) fn root_construction_has_unclassified(construction: &RootConstruction) -> bool {
    !construction.unclassified.is_empty()
}

pub(crate) fn acknowledge_preserved_root_construction(construction: RootConstruction) {
    debug_assert!(!construction.unclassified.is_empty());
    drop(construction);
}

pub(crate) fn root_construction_guard(construction: &RootConstruction) -> io::Result<&RootGuard> {
    if !construction.unclassified.is_empty()
        || construction
            .created
            .iter()
            .any(|binding| !binding.published)
    {
        return Err(io::Error::other(
            "application root construction retains unpublished debris",
        ));
    }
    let guard = construction
        .guard
        .as_ref()
        .ok_or_else(|| io::Error::other("application root construction is not complete"))?;
    validate_root(guard)?;
    Ok(guard)
}

pub(crate) fn root_construction_identity(construction: &RootConstruction) -> io::Result<Identity> {
    Ok(root_construction_guard(construction)?.identity)
}

pub(crate) fn finish_root_construction(mut construction: RootConstruction) -> RootGuard {
    assert!(
        construction.unclassified.is_empty()
            && construction.created.iter().all(|binding| binding.published),
        "only a fully classified root construction can be finished"
    );
    construction
        .guard
        .take()
        .expect("completed root construction retains its guard")
}

pub(crate) fn reconcile_root_construction(
    mut construction: RootConstruction,
) -> Result<RootConstruction, RootConstructionError> {
    while let Some(creation) = construction.unclassified.pop() {
        match classify_or_settle_root_creation(creation) {
            Ok(Some(binding)) => construction.created.push(binding),
            Ok(None) => {}
            Err((error, creation)) => {
                construction.unclassified.push(creation);
                return Err(RootConstructionError {
                    error,
                    construction: Some(construction),
                });
            }
        }
    }
    for binding in &mut construction.created {
        match directory_binding_state(&binding.parent, &binding.name, binding.identity) {
            Ok(BindingState::Exact) => {}
            Ok(BindingState::Absent | BindingState::Occupied) => binding.published = false,
            Err(error) => {
                return Err(RootConstructionError {
                    error,
                    construction: Some(construction),
                });
            }
        }
    }
    let mut unpublished_bindings = Vec::new();
    for binding in std::mem::take(&mut construction.created) {
        if binding.published {
            construction.created.push(binding);
        } else {
            unpublished_bindings.push(binding);
        }
    }
    if !unpublished_bindings.is_empty() {
        let debris = RootConstruction {
            target: construction.target.clone(),
            guard: None,
            created: unpublished_bindings,
            unclassified: Vec::new(),
        };
        if let Err(error) = cleanup_root_construction(debris) {
            let (error, debris) = error.into_parts();
            if let Some(mut debris) = debris {
                construction.created.append(&mut debris.created);
                construction.unclassified.append(&mut debris.unclassified);
            }
            return Err(RootConstructionError {
                error,
                construction: Some(construction),
            });
        }
    }
    if construction
        .guard
        .as_ref()
        .is_some_and(|guard| validate_root(guard).is_ok())
    {
        return Ok(construction);
    }
    let target = construction.target.clone();
    match open_or_create_root(&target) {
        Ok(mut next) => {
            construction.created.append(&mut next.created);
            construction.unclassified.append(&mut next.unclassified);
            next.created = construction.created;
            next.unclassified = construction.unclassified;
            Ok(next)
        }
        Err(error) => {
            let (error, next) = error.into_parts();
            if let Some(mut next) = next {
                construction.created.append(&mut next.created);
                construction.unclassified.append(&mut next.unclassified);
            }
            construction.guard = None;
            Err(RootConstructionError {
                error,
                construction: Some(construction),
            })
        }
    }
}

pub(crate) fn cleanup_root_construction(
    mut construction: RootConstruction,
) -> Result<(), RootConstructionError> {
    construction.guard.take();
    while let Some(creation) = construction.unclassified.pop() {
        match classify_or_settle_root_creation(creation) {
            Ok(Some(binding)) => construction.created.push(binding),
            Ok(None) => {}
            Err((error, creation)) => {
                construction.unclassified.push(creation);
                return Err(RootConstructionError {
                    error,
                    construction: Some(construction),
                });
            }
        }
    }
    while let Some(mut binding) = construction.created.pop() {
        let cleanup = (|| {
            let state = directory_binding_state(&binding.parent, &binding.name, binding.identity)?;
            match state {
                BindingState::Absent | BindingState::Occupied => {
                    let Some(child) = binding.child.as_ref() else {
                        return Err(binding_changed(
                            "created root directory moved without retained identity authority",
                        ));
                    };
                    if !retained_directory_is_removed(child, binding.identity)? {
                        return Err(binding_changed(
                            "created root directory moved before cleanup",
                        ));
                    }
                    sync_directory(&binding.parent)?;
                    return if retained_directory_is_removed(child, binding.identity)? {
                        Ok(())
                    } else {
                        Err(binding_changed(
                            "created root cleanup unlink proof did not remain stable",
                        ))
                    };
                }
                BindingState::Exact => {}
            }
            if binding.child.is_none() {
                let child = rfs::openat(
                    &binding.parent,
                    &binding.name,
                    directory_flags(),
                    Mode::empty(),
                )?;
                if directory_identity(&child)? != binding.identity {
                    return Err(binding_changed(
                        "created root directory changed before cleanup admission",
                    ));
                }
                binding.child = Some(child);
            }
            let child = binding
                .child
                .as_ref()
                .expect("created root cleanup retains its child");
            if directory_identity(child)? != binding.identity
                || !entries(child, 1)?.entries.is_empty()
            {
                return Err(binding_changed(
                    "created root directory changed before cleanup",
                ));
            }
            rfs::unlinkat(&binding.parent, &binding.name, AtFlags::REMOVEDIR)?;
            sync_directory(&binding.parent)?;
            if !retained_directory_is_removed(child, binding.identity)? {
                return Err(binding_changed(
                    "created root directory cleanup was not exact",
                ));
            }
            Ok(())
        })();
        if let Err(error) = cleanup {
            construction.created.push(binding);
            return Err(RootConstructionError {
                error,
                construction: Some(construction),
            });
        }
    }
    Ok(())
}

fn classify_or_settle_root_creation(
    mut creation: RootCreationReservation,
) -> Result<Option<RootCreatedBinding>, (io::Error, RootCreationReservation)> {
    let identity = match creation.child.as_ref() {
        Some(child) => match directory_identity(child) {
            Ok(identity) => Some(identity),
            Err(error) => return Err((error, creation)),
        },
        None => None,
    };
    let observation = match entry_observation(&creation.parent, &creation.name) {
        Ok(observation) => observation,
        Err(error) => return Err((error, creation)),
    };
    match (identity, observation) {
        (Some(identity), Some((EntryKind::Directory, observed))) if observed == identity => {
            let child = creation.child.take();
            Ok(Some(RootCreatedBinding {
                parent: creation.parent,
                name: creation.name,
                identity,
                child,
                published: creation.published,
            }))
        }
        (Some(identity), _) => {
            let child = creation
                .child
                .take()
                .expect("classified root creation retains its child");
            if let Ok(true) = retained_directory_is_removed(&child, identity) {
                return Ok(None);
            }
            Ok(Some(RootCreatedBinding {
                parent: creation.parent,
                name: creation.name,
                identity,
                child: Some(child),
                published: false,
            }))
        }
        _ => Err((
            binding_changed("unclassified root creation could not be proven exact"),
            creation,
        )),
    }
}

fn retained_directory_is_removed(child: &DirectoryHandle, expected: Identity) -> io::Result<bool> {
    let stat = rfs::fstat(child)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || identity_from_stat(stat) != expected
    {
        return Err(binding_changed("retained directory changed identity"));
    }
    #[cfg(target_os = "linux")]
    return Ok(stat.st_nlink == 0);
    // Darwin retains link count two and a stale F_GETPATH after rmdir.
    #[cfg(target_os = "macos")]
    return Ok(retained_directory_path_identity(child)? != Some(expected));
}

#[cfg(target_os = "macos")]
fn retained_directory_path_identity(child: &DirectoryHandle) -> io::Result<Option<Identity>> {
    let mut path = [0_u8; libc::PATH_MAX as usize];
    loop {
        let result = unsafe {
            libc::fcntl(
                child.as_raw_fd(),
                libc::F_GETPATH,
                path.as_mut_ptr().cast::<libc::c_char>(),
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let path = CStr::from_bytes_until_nul(&path)
        .map_err(|_| binding_changed("retained directory path was not terminated"))?;
    match rfs::stat(OsStr::from_bytes(path.to_bytes())) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
            Ok(Some(identity_from_stat(stat)))
        }
        Ok(_) => Ok(None),
        Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn clone_root(root: &RootGuard) -> io::Result<DirectoryHandle> {
    Ok(rfs::openat(
        &root.handle,
        ".",
        directory_flags(),
        Mode::empty(),
    )?)
}

fn retain_root_proof(root: &RootGuard) -> io::Result<RootGuard> {
    validate_root(root)?;
    let bindings = root
        .bindings
        .iter()
        .map(|binding| {
            Ok(RootBinding {
                parent: clone_directory_handle(&binding.parent)?,
                name: binding.name.clone(),
                identity: binding.identity,
                exact_name: binding.exact_name,
                exact_revision: Arc::clone(&binding.exact_revision),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let retained = RootGuard {
        handle: clone_directory_handle(&root.handle)?,
        identity: root.identity,
        bindings,
    };
    validate_root(&retained)?;
    Ok(retained)
}

pub(crate) fn validate_root(root: &RootGuard) -> io::Result<()> {
    validate_root_handle(root)?;
    for binding in &root.bindings {
        let state = if binding.exact_name {
            exact_directory_binding_state(
                &binding.parent,
                &binding.name,
                binding.identity,
                &binding.exact_revision,
            )?
        } else {
            directory_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(binding_changed("application root ancestry changed binding"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_root_preallocated(
    root: &RootGuard,
    buffer: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<()> {
    if directory_identity_preallocated(&root.handle)? != root.identity {
        return Err(io::ErrorKind::InvalidData.into());
    }
    for binding in &root.bindings {
        let state = if binding.exact_name {
            exact_directory_binding_state_preallocated(
                &binding.parent,
                &binding.name,
                binding.identity,
                buffer,
            )?
        } else {
            directory_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
    Ok(())
}

pub(crate) fn validate_root_handle(root: &RootGuard) -> io::Result<()> {
    if directory_identity(&root.handle)? == root.identity {
        Ok(())
    } else {
        Err(binding_changed("application root handle changed identity"))
    }
}

pub(crate) fn clear_root_children(
    root: &RootGuard,
    lease: &LeaseHandle,
    lease_name: &OsStr,
    preserved: &[(&OsStr, Identity)],
) -> io::Result<()> {
    validate_lease_binding(
        root,
        lease_name,
        &lease.handle,
        lease.identity,
        &lease.name_class_revision,
    )?;
    let anchors: Vec<_> = std::iter::once((lease_name, lease.identity))
        .chain(preserved.iter().copied())
        .collect();
    prove_root_anchors(root, &anchors)?;
    clear_directory_children(&root.handle, &anchors)?;
    sync_directory(&root.handle)?;
    prove_root_children_cleared(root, lease, &anchors)
}

fn clear_directory_children(
    root: &DirectoryHandle,
    preserved_root_entries: &[(&OsStr, Identity)],
) -> io::Result<()> {
    struct ClearFrame {
        directory: DirectoryHandle,
        entries: Vec<(OsString, EntryKind)>,
        depth: usize,
        remove: Option<(OsString, Identity)>,
    }

    let root_listing = entries(root, MAX_TREE_CLEAR_ENTRIES + 1)?;
    if !root_listing.complete {
        return Err(io::Error::other(
            "directory tree entry count exceeds bounded capacity",
        ));
    }
    let mut total_entries = root_listing.entries.len();
    if total_entries > MAX_TREE_CLEAR_ENTRIES {
        return Err(io::Error::other(
            "directory tree entry count exceeds bounded capacity",
        ));
    }
    let mut stack = vec![ClearFrame {
        directory: clone_directory_handle(root)?,
        entries: root_listing.entries,
        depth: 0,
        remove: None,
    }];
    while let Some(frame) = stack.last_mut() {
        if let Some((name, kind)) = frame.entries.pop() {
            if let Some((_, identity)) = preserved_root_entries.iter().find(|(preserved, _)| {
                frame.depth == 0 && super::leaf_names_equal(&name, preserved)
            }) {
                if kind != EntryKind::File
                    || entry_observation(&frame.directory, &name)?
                        != Some((EntryKind::File, *identity))
                {
                    return Err(binding_changed("preserved root entry changed"));
                }
                continue;
            }
            let (observed_kind, observed_identity) = entry_observation(&frame.directory, &name)?
                .ok_or_else(|| {
                    binding_changed("directory tree entry disappeared before admission")
                })?;
            if observed_kind != kind {
                return Err(binding_changed(
                    "directory tree entry changed classification",
                ));
            }
            if observed_kind == EntryKind::Directory {
                let child_depth = frame
                    .depth
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("directory tree depth overflowed"))?;
                if child_depth > MAX_TREE_CLEAR_DEPTH {
                    return Err(io::Error::other(
                        "directory tree depth exceeds bounded capacity",
                    ));
                }
                let (child, identity) = open_directory(&frame.directory, &name)?;
                if identity != observed_identity {
                    return Err(binding_changed(
                        "directory tree child changed before admission",
                    ));
                }
                let listing = entries(&child, MAX_TREE_CLEAR_ENTRIES + 1)?;
                if !listing.complete {
                    return Err(io::Error::other(
                        "directory tree entry count exceeds bounded capacity",
                    ));
                }
                total_entries = total_entries
                    .checked_add(listing.entries.len())
                    .ok_or_else(|| io::Error::other("directory tree entry count overflowed"))?;
                if total_entries > MAX_TREE_CLEAR_ENTRIES {
                    return Err(io::Error::other(
                        "directory tree entry count exceeds bounded capacity",
                    ));
                }
                stack.push(ClearFrame {
                    directory: child,
                    entries: listing.entries,
                    depth: child_depth,
                    remove: Some((name, identity)),
                });
            } else {
                remove_tree_leaf(&frame.directory, &name, observed_kind, observed_identity)?;
            }
            continue;
        }
        let completed = stack.pop().expect("tree clear frame remains present");
        let Some((name, identity)) = completed.remove else {
            break;
        };
        let parent = &stack
            .last()
            .expect("non-root tree clear frame retains its parent")
            .directory;
        if directory_binding_state(parent, &name, identity)? != BindingState::Exact {
            return Err(binding_changed(
                "directory tree child changed before removal",
            ));
        }
        rfs::unlinkat(parent, &name, AtFlags::REMOVEDIR)?;
        if !retained_directory_is_removed(&completed.directory, identity)? {
            return Err(binding_changed("directory tree removal was not exact"));
        }
    }
    Ok(())
}

fn remove_tree_leaf(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected_kind: EntryKind,
    expected_identity: Identity,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let retained = rfs::openat(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let observed_kind = match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Directory,
        FileType::Symlink => EntryKind::Link,
        _ => EntryKind::Other,
    };
    if observed_kind != expected_kind
        || observed_kind == EntryKind::Directory
        || identity_from_stat(stat) != expected_identity
    {
        return Err(binding_changed(
            "directory tree entry changed before removal",
        ));
    }
    #[cfg(target_os = "linux")]
    if identity_from_stat(rfs::fstat(&retained)?) != expected_identity {
        return Err(binding_changed(
            "directory tree retained entry changed before removal",
        ));
    }
    // CapabilityAuthority and the root lease serialize cooperating writers.
    // Linux offers no unprivileged fd-targeted unlink, so a process racing
    // this private name outside that authority cannot be excluded here.
    rfs::unlinkat(parent, name, AtFlags::empty())?;
    #[cfg(target_os = "linux")]
    if identity_from_stat(rfs::fstat(&retained)?) != expected_identity {
        return Err(binding_changed(
            "directory tree retained entry changed after removal",
        ));
    }
    if entry_observation(parent, name)?.is_some() {
        return Err(binding_changed(
            "directory tree entry removal was not exact",
        ));
    }
    Ok(())
}

fn prove_root_children_cleared(
    root: &RootGuard,
    lease: &LeaseHandle,
    anchors: &[(&OsStr, Identity)],
) -> io::Result<()> {
    let listing = entries(&root.handle, anchors.len() + 1)?;
    if !listing.complete || listing.entries.len() != anchors.len() {
        return Err(binding_changed("reset root final listing is incomplete"));
    }
    prove_root_anchors(root, anchors)?;
    validate_root(root)?;
    validate_lease(lease)
}

fn prove_root_anchors(root: &RootGuard, anchors: &[(&OsStr, Identity)]) -> io::Result<()> {
    for (name, identity) in anchors {
        if entry_observation(&root.handle, name)? != Some((EntryKind::File, *identity)) {
            return Err(binding_changed("reset preserved entry changed"));
        }
    }
    Ok(())
}

pub(crate) fn lease_identity_witness(lease: &LeaseHandle) -> [u8; 32] {
    identity_witness(lease.identity)
}

pub(crate) fn finish_root_reset(
    root: &RootGuard,
    lease: &LeaseHandle,
    marker: (&OsStr, Identity),
    intent: (&OsStr, Identity, &File),
) -> io::Result<()> {
    validate_root(root)?;
    validate_lease(lease)?;
    let anchors = [(lease.name.as_os_str(), lease.identity), marker];
    match entry_observation(&root.handle, intent.0)? {
        Some((EntryKind::File, identity)) if identity == intent.1 => {
            prove_root_children_cleared(
                root,
                lease,
                &[anchors[0], anchors[1], (intent.0, intent.1)],
            )?;
            sync_directory(&root.handle)?;
            remove_tree_leaf(&root.handle, intent.0, EntryKind::File, intent.1)?;
        }
        None if retained_file_identity(intent.2)? == (intent.1, 0) => {}
        _ => return Err(binding_changed("reset intent changed before completion")),
    }
    sync_directory(&root.handle)?;
    prove_root_children_cleared(root, lease, &anchors)
}

pub(crate) fn directory_identity(handle: &DirectoryHandle) -> io::Result<Identity> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem capability is not a directory",
        ));
    }
    Ok(identity_from_stat(stat))
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_identity_preallocated(handle: &DirectoryHandle) -> io::Result<Identity> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(identity_from_stat(stat))
}

pub(crate) fn directory_revision(handle: &DirectoryHandle) -> io::Result<DirectoryStamp> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem capability is not a directory",
        ));
    }
    let modified_nanos = i64::try_from(stat.st_mtime_nsec)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mtime is out of range"))?;
    let changed_nanos = i64::try_from(stat.st_ctime_nsec)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ctime is out of range"))?;
    Ok(DirectoryStamp {
        modified_seconds: stat.st_mtime,
        modified_nanos,
        changed_seconds: stat.st_ctime,
        changed_nanos,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_created_at_ns(handle: &DirectoryHandle) -> io::Result<u64> {
    directory_identity(handle)?;
    let stat = match rfs::statx(handle, "", AtFlags::EMPTY_PATH, rfs::StatxFlags::BTIME) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem birth time is unavailable",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if stat.stx_mask & rfs::StatxFlags::BTIME.bits() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem birth time is unavailable",
        ));
    }
    birth_time_ns(stat.stx_btime.tv_sec, i64::from(stat.stx_btime.tv_nsec))
}

#[cfg(target_os = "macos")]
pub(crate) fn directory_created_at_ns(handle: &DirectoryHandle) -> io::Result<u64> {
    directory_identity(handle)?;
    let stat = rfs::fstat(handle)?;
    birth_time_ns(stat.st_birthtime, stat.st_birthtime_nsec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn directory_created_at_ns(_handle: &DirectoryHandle) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem birth time is unavailable",
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn directory_revision_preallocated(
    handle: &DirectoryHandle,
) -> io::Result<DirectoryStamp> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let modified_nanos =
        i64::try_from(stat.st_mtime_nsec).map_err(|_| io::ErrorKind::InvalidData)?;
    let changed_nanos =
        i64::try_from(stat.st_ctime_nsec).map_err(|_| io::ErrorKind::InvalidData)?;
    Ok(DirectoryStamp {
        modified_seconds: stat.st_mtime,
        modified_nanos,
        changed_seconds: stat.st_ctime,
        changed_nanos,
    })
}

pub(crate) fn open_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<(DirectoryHandle, Identity)> {
    let handle = rfs::openat(parent, name, directory_flags(), Mode::empty())?;
    let identity = directory_identity(&handle)?;
    Ok((handle, identity))
}

pub(crate) fn create_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> Result<DirectoryHandle, CreateDirectoryError> {
    rfs::mkdirat(parent, name, Mode::from_bits_truncate(0o700))
        .map_err(|error| CreateDirectoryError::NoEffect(error.into()))?;
    match rfs::openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(handle) => Ok(handle),
        Err(error) => Err(CreateDirectoryError::CreatedUnclassified(error.into())),
    }
}

pub(crate) fn create_symlink(
    parent: &DirectoryHandle,
    name: &OsStr,
    target: &OsStr,
) -> io::Result<()> {
    rfs::symlinkat(Path::new(target), parent, name)?;
    Ok(())
}

pub(crate) fn read_symlink(parent: &DirectoryHandle, name: &OsStr) -> io::Result<OsString> {
    let target = rfs::readlinkat(parent, name, Vec::new())?;
    Ok(OsString::from_vec(target.into_bytes()))
}

pub(crate) fn open_file(parent: &DirectoryHandle, name: &OsStr) -> io::Result<File> {
    let handle = rfs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    require_regular_file(&handle)?;
    Ok(File::from(handle))
}

pub(crate) fn open_recoverable_stage(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<File> {
    if file_binding_state(parent, name, expected)? != BindingState::Exact {
        return Err(binding_changed("recovery stage changed before reopen"));
    }
    rfs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(Into::into)
}

pub(crate) fn create_file(parent: &DirectoryHandle, name: &OsStr) -> Result<File, CreateFileError> {
    let handle = rfs::openat(
        parent,
        name,
        OFlags::RDWR
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| CreateFileError::NoEffect(error.into()))?;
    if let Err(error) = require_regular_file(&handle) {
        return Err(CreateFileError::AppliedUnverified {
            error,
            retained: File::from(handle),
        });
    }
    Ok(File::from(handle))
}

#[cfg(target_os = "linux")]
pub(crate) fn create_transient_file(
    parent: &DirectoryHandle,
) -> Result<(TransientFile, Identity), CreateTransientFileError> {
    create_linux_transient_file(parent).map_err(CreateTransientFileError::NoEffect)
}

#[cfg(target_os = "linux")]
fn create_linux_transient_file(parent: &DirectoryHandle) -> io::Result<(TransientFile, Identity)> {
    let handle = rfs::openat(
        parent,
        ".",
        OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| {
        let error = io::Error::from(error);
        let unsupported = error.raw_os_error().is_some_and(|code| {
            [libc::EISDIR, libc::EINVAL, libc::EOPNOTSUPP, libc::ENOSYS].contains(&code)
        });
        if unsupported {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "the destination filesystem does not support anonymous transient files",
            )
        } else {
            error
        }
    })?;
    let file = File::from(handle);
    let (identity, links) = retained_file_identity(&file)?;
    if links != 0 {
        return Err(binding_changed(
            "anonymous transient file unexpectedly has a namespace link",
        ));
    }
    let proc_path = linux_transient_proc_path(&file);
    let proc_identity = rfs::stat(&proc_path).map_err(|error| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("procfs cannot resolve the anonymous transient file: {error}"),
        )
    })?;
    if identity_from_stat(proc_identity) != identity {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "procfs cannot resolve the anonymous transient file",
        ));
    }
    Ok((TransientFile { file, proc_path }, identity))
}

#[cfg(target_os = "linux")]
fn linux_transient_proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "linux")]
pub(crate) fn read_transient_at(
    transient: &TransientFile,
    bytes: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    transient.file.read_at(bytes, offset)
}

#[cfg(target_os = "linux")]
pub(crate) fn write_transient_at(
    transient: &TransientFile,
    bytes: &[u8],
    offset: u64,
) -> io::Result<usize> {
    transient.file.write_at(bytes, offset)
}

#[cfg(target_os = "linux")]
pub(crate) fn seal_transient_file(
    transient: &mut TransientFile,
    expected: Identity,
    size: u64,
) -> io::Result<()> {
    let (identity, links) = retained_file_identity(&transient.file)?;
    if identity != expected || links != 0 || transient.file.metadata()?.len() != size {
        return Err(binding_changed(
            "anonymous transient file changed while it was being sealed",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn link_transient_file(
    transient: &mut TransientFile,
    parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let (_, links) = retained_file_identity_preallocated(&transient.file)?;
    if links != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // Scratch bytes become durable only when they are published.
    transient.file.sync_all()?;
    rfs::linkat(
        rfs::CWD,
        &transient.proc_path,
        parent,
        destination_name,
        AtFlags::SYMLINK_FOLLOW,
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn transient_publication_state_preallocated(
    transient: &TransientFile,
    parent: &DirectoryHandle,
    destination_name: &OsStr,
    expected: Identity,
) -> io::Result<TransientPublicationState> {
    let (identity, links) = retained_file_identity_preallocated(&transient.file)?;
    if identity != expected {
        return Ok(TransientPublicationState::Indeterminate);
    }
    let destination = file_binding_state(parent, destination_name, expected)?;
    Ok(match (links, destination) {
        (0, BindingState::Absent | BindingState::Occupied) => {
            TransientPublicationState::Unpublished
        }
        (1, BindingState::Exact) => TransientPublicationState::Published,
        _ => TransientPublicationState::Indeterminate,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn transient_file_evidence(transient: &TransientFile) -> io::Result<(Identity, u64)> {
    retained_file_identity(&transient.file)
}

#[cfg(target_os = "linux")]
pub(crate) fn transient_file_evidence_preallocated(
    transient: &TransientFile,
) -> io::Result<(Identity, u64)> {
    retained_file_identity_preallocated(&transient.file)
}

#[cfg(target_os = "linux")]
pub(crate) fn into_published_file(transient: TransientFile) -> File {
    transient.file
}

#[cfg(target_os = "linux")]
pub(crate) fn discard_transient_file(
    transient: TransientFile,
    expected: Identity,
) -> Result<(), DiscardTransientFileError> {
    match retained_file_identity(&transient.file) {
        Ok((identity, 0)) if identity == expected => {}
        Ok(_) => {
            return Err(DiscardTransientFileError::Retained {
                error: binding_changed("anonymous transient has an external link"),
                file: transient,
            });
        }
        Err(error) => {
            return Err(DiscardTransientFileError::Retained {
                error,
                file: transient,
            });
        }
    }
    drop(transient);
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn discard_transient_file_preallocated(
    transient: TransientFile,
    expected: Identity,
) -> Result<(), DiscardTransientFileError> {
    match retained_file_identity_preallocated(&transient.file) {
        Ok((identity, 0)) if identity == expected => {}
        Ok(_) => {
            return Err(DiscardTransientFileError::Retained {
                error: io::ErrorKind::InvalidData.into(),
                file: transient,
            });
        }
        Err(error) => {
            return Err(DiscardTransientFileError::Retained {
                error,
                file: transient,
            });
        }
    }
    drop(transient);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn named_stage_evidence(file: &File) -> io::Result<(Identity, u64)> {
    retained_file_identity(file)
}

pub(crate) fn clone_stage_cleanup(
    parent: &DirectoryHandle,
    name: &OsStr,
    stage: &File,
    expected: Identity,
) -> io::Result<FileCleanupHandle> {
    if file_identity(stage)? != expected
        || file_binding_state(parent, name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("created stage changed before registration"));
    }
    Ok(FileCleanupHandle(stage.try_clone()?))
}

pub(crate) fn remove_recoverable_stage(
    parent: &DirectoryHandle,
    name: &OsStr,
    stage: &mut File,
    expected: Identity,
) -> io::Result<()> {
    if file_identity(stage)? != expected
        || file_binding_state(parent, name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("recovery stage changed before removal"));
    }
    rfs::unlinkat(parent, name, AtFlags::empty())?;
    Ok(())
}

pub(crate) fn rename_recovery_file_no_replace(
    parent: &DirectoryHandle,
    source_name: &OsStr,
    file: &File,
    expected: Identity,
    destination_name: &OsStr,
) -> io::Result<()> {
    if file_identity(file)? != expected
        || file_binding_state(parent, source_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("recovery file changed before rename"));
    }
    rfs::renameat_with(
        parent,
        source_name,
        parent,
        destination_name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

pub(crate) fn settle_renamed_recovery_file(
    parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_name: &OsStr,
    file: &File,
    expected: Identity,
) -> io::Result<()> {
    sync_directory(parent)?;
    if file_identity(file)? != expected
        || file_binding_state(parent, source_name, expected)? != BindingState::Absent
        || file_binding_state(parent, destination_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("recovery file rename was not exact"));
    }
    Ok(())
}

pub(crate) fn settle_removed_recoverable_stage(
    parent: &DirectoryHandle,
    name: &OsStr,
    stage: &File,
    expected: Identity,
) -> io::Result<()> {
    sync_directory(parent)?;
    let (identity, links) = retained_file_identity(stage)?;
    if identity != expected
        || links != 0
        || file_binding_state(parent, name, expected)? != BindingState::Absent
    {
        return Err(binding_changed("recovery stage removal was not exact"));
    }
    Ok(())
}

pub(crate) fn file_identity(file: &File) -> io::Result<Identity> {
    let stat = require_regular_file(file)?;
    Ok(identity_from_stat(stat))
}

pub(crate) fn make_file_executable(file: &File) -> io::Result<()> {
    require_regular_file(file)?;
    rfs::fchmod(file, Mode::from_bits_truncate(0o755))?;
    let stat = rfs::fstat(file)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_mode & 0o777 != 0o755
    {
        return Err(binding_changed(
            "executable file permissions changed unexpectedly",
        ));
    }
    Ok(())
}

pub(crate) fn file_is_executable(file: &File) -> io::Result<bool> {
    require_regular_file(file)?;
    Ok(rfs::fstat(file)?.st_mode & 0o111 != 0)
}

pub(crate) fn file_receipt_fields(file: &File) -> io::Result<(u64, FileStamp)> {
    let stat = require_regular_file(file)?;
    let size = u64::try_from(stat.st_size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file size is negative"))?;
    let modified_nanos = i64::try_from(stat.st_mtime_nsec)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mtime is out of range"))?;
    let changed_nanos = i64::try_from(stat.st_ctime_nsec)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ctime is out of range"))?;
    Ok((
        size,
        FileStamp {
            modified_seconds: stat.st_mtime,
            modified_nanos,
            changed_seconds: stat.st_ctime,
            changed_nanos,
        },
    ))
}

pub(crate) fn file_content_stamp_matches(first: FileStamp, second: FileStamp) -> bool {
    first.modified_seconds == second.modified_seconds
        && first.modified_nanos == second.modified_nanos
}

pub(crate) fn file_modified_at_ns(stamp: FileStamp) -> io::Result<u64> {
    let modified_seconds = u64::try_from(stamp.modified_seconds)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mtime precedes the Unix epoch"))?;
    let modified_nanos = u64::try_from(stamp.modified_nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime nanos are invalid"))?;
    let modified_at_ns = modified_seconds
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(modified_nanos))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime overflowed"))?;
    Ok(modified_at_ns)
}

pub(crate) fn file_changed_at_ns(stamp: FileStamp) -> io::Result<u64> {
    let changed_seconds = u64::try_from(stamp.changed_seconds)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ctime precedes the Unix epoch"))?;
    let changed_nanos = u64::try_from(stamp.changed_nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ctime nanos are invalid"))?;
    let changed_at_ns = changed_seconds
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(changed_nanos))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ctime overflowed"))?;
    Ok(changed_at_ns)
}

pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(bytes, offset)
}

pub(crate) fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    file.write_at(bytes, offset)
}

fn entry_observation(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<Option<(EntryKind, Identity)>> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some((
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => EntryKind::File,
                FileType::Directory => EntryKind::Directory,
                FileType::Symlink => EntryKind::Link,
                _ => EntryKind::Other,
            },
            identity_from_stat(stat),
        ))),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn visit_entries<F>(
    parent: &DirectoryHandle,
    limit: usize,
    mut visitor: F,
) -> io::Result<VisitCompletion>
where
    F: FnMut(&OsStr, EntryKind) -> io::Result<ControlFlow<()>>,
{
    let mut directory = Dir::read_from(parent)?;
    let mut observed = 0_usize;
    loop {
        let Some(entry) = directory.next() else {
            return Ok(VisitCompletion::Complete);
        };
        let entry = entry?;
        let raw_name: &CStr = entry.file_name();
        if matches!(raw_name.to_bytes(), b"." | b"..") {
            continue;
        }
        if observed == limit {
            return Ok(VisitCompletion::LimitExceeded);
        }
        let borrowed_name = OsStr::from_bytes(raw_name.to_bytes());
        let kind = match entry.file_type() {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Link,
            FileType::Unknown => match entry_observation(parent, borrowed_name)? {
                Some((kind, _)) => kind,
                None => continue,
            },
            _ => EntryKind::Other,
        };
        if visitor(borrowed_name, kind)?.is_break() {
            return Ok(VisitCompletion::Stopped);
        }
        observed += 1;
    }
}

#[cfg(target_os = "linux")]
pub(crate) const TRANSIENT_DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
pub(crate) fn visit_entries_preallocated<F>(
    parent: &DirectoryHandle,
    buffer: &mut [std::mem::MaybeUninit<u8>],
    limit: usize,
    mut visitor: F,
) -> io::Result<VisitCompletion>
where
    F: FnMut(&OsStr, EntryKind) -> io::Result<ControlFlow<()>>,
{
    let handle = rfs::openat(parent, ".", directory_flags(), Mode::empty())?;
    let mut directory = rfs::RawDir::new(handle, buffer);
    let mut observed = 0_usize;
    loop {
        let Some(entry) = directory.next() else {
            return Ok(VisitCompletion::Complete);
        };
        let entry = entry?;
        let raw_name: &CStr = entry.file_name();
        if matches!(raw_name.to_bytes(), b"." | b"..") {
            continue;
        }
        if observed == limit {
            return Ok(VisitCompletion::LimitExceeded);
        }
        let borrowed_name = OsStr::from_bytes(raw_name.to_bytes());
        let kind = match entry.file_type() {
            FileType::RegularFile => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Link,
            FileType::Unknown => match entry_observation(parent, borrowed_name)? {
                Some((kind, _)) => kind,
                None => continue,
            },
            _ => EntryKind::Other,
        };
        if visitor(borrowed_name, kind)?.is_break() {
            return Ok(VisitCompletion::Stopped);
        }
        observed += 1;
    }
}

pub(crate) fn entries(parent: &DirectoryHandle, limit: usize) -> io::Result<DirectoryEntries> {
    let mut entries = Vec::new();
    let completion = visit_entries(parent, limit, |name, kind| {
        entries.push((name.to_os_string(), kind));
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(DirectoryEntries {
        entries,
        complete: completion == VisitCompletion::Complete,
    })
}

pub(crate) fn file_binding_state(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<BindingState> {
    match entry_observation(parent, name)? {
        None => Ok(BindingState::Absent),
        Some((EntryKind::File, identity)) if identity == expected => Ok(BindingState::Exact),
        Some(_) => Ok(BindingState::Occupied),
    }
}

/// Whether `name` binds `expected` as an exact single-link regular file.
pub(crate) fn exact_file_binding(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<bool> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_nlink == 1
                && identity_from_stat(stat) == expected,
        ),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn exact_file_link_count(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<Option<u64>> {
    let file = match open_file(parent, name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let (identity, links) = retained_file_identity(&file)?;
    if identity != expected || file_binding_state(parent, name, expected)? != BindingState::Exact {
        return Ok(None);
    }
    Ok(Some(links))
}

pub(crate) fn directory_binding_state(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<BindingState> {
    match entry_observation(parent, name)? {
        None => Ok(BindingState::Absent),
        Some((EntryKind::Directory, identity)) if identity == expected => Ok(BindingState::Exact),
        Some(_) => Ok(BindingState::Occupied),
    }
}

fn exact_directory_name_observation(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<BindingState> {
    let mut directory = Dir::read_from(parent)?;
    let mut observed = 0usize;
    loop {
        let Some(entry) = directory.next() else {
            return Ok(BindingState::Occupied);
        };
        let entry = entry?;
        let raw_name: &CStr = entry.file_name();
        if matches!(raw_name.to_bytes(), b"." | b"..") {
            continue;
        }
        if observed == crate::MAX_DIRECTORY_LIST_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact directory binding parent exceeds its entry bound",
            ));
        }
        observed += 1;
        let observed_name = OsStr::from_bytes(raw_name.to_bytes());
        if observed_name != name {
            continue;
        }
        return match entry_observation(parent, observed_name)? {
            Some((EntryKind::Directory, identity)) if identity == expected => {
                Ok(BindingState::Exact)
            }
            _ => Ok(BindingState::Occupied),
        };
    }
}

fn exact_directory_binding_state(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
    cached_revision: &RwLock<Option<DirectoryStamp>>,
) -> io::Result<BindingState> {
    let state = directory_binding_state(parent, name, expected)?;
    if state != BindingState::Exact {
        return Ok(state);
    }
    let observed_revision = directory_revision(parent)?;
    if cached_revision
        .read()
        .map_err(|_| io::Error::other("exact directory binding proof lock is poisoned"))?
        .as_ref()
        == Some(&observed_revision)
    {
        return Ok(BindingState::Exact);
    }
    let mut cached_revision = cached_revision
        .write()
        .map_err(|_| io::Error::other("exact directory binding proof lock is poisoned"))?;
    for attempt in 0..EXACT_DIRECTORY_BINDING_VALIDATION_ATTEMPTS {
        let state = directory_binding_state(parent, name, expected)?;
        if state != BindingState::Exact {
            return Ok(state);
        }
        let revision = directory_revision(parent)?;
        if cached_revision.as_ref() == Some(&revision) {
            return Ok(BindingState::Exact);
        }
        *cached_revision = None;
        let exact_state = exact_directory_name_observation(parent, name, expected)?;
        #[cfg(test)]
        run_exact_directory_binding_test_hook(name);
        let final_state = directory_binding_state(parent, name, expected)?;
        if final_state != BindingState::Exact {
            return Ok(final_state);
        }
        let final_revision = directory_revision(parent)?;
        if exact_state == BindingState::Exact {
            if final_revision == revision {
                *cached_revision = Some(revision);
                return Ok(BindingState::Exact);
            }
            let confirmed = exact_directory_name_observation(parent, name, expected)?;
            let confirmed_state = directory_binding_state(parent, name, expected)?;
            if confirmed_state != BindingState::Exact {
                return Ok(confirmed_state);
            }
            if confirmed == BindingState::Exact {
                return Ok(BindingState::Exact);
            }
            if directory_revision(parent)? != final_revision {
                if attempt + 1 == EXACT_DIRECTORY_BINDING_VALIDATION_ATTEMPTS {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "exact directory binding name remained unobservable during parent churn",
                    ));
                }
                continue;
            }
            return Ok(confirmed);
        }
        if final_revision != revision {
            if attempt + 1 == EXACT_DIRECTORY_BINDING_VALIDATION_ATTEMPTS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "exact directory binding name remained unobservable during parent churn",
                ));
            }
            continue;
        }
        return Ok(exact_state);
    }
    unreachable!("exact directory binding retry loop has a non-zero bound")
}

#[cfg(target_os = "linux")]
fn exact_directory_name_observation_preallocated(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
    buffer: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<BindingState> {
    let mut exact_state = BindingState::Occupied;
    let completion = visit_entries_preallocated(
        parent,
        buffer,
        crate::MAX_DIRECTORY_LIST_ENTRIES,
        |observed_name, kind| {
            if observed_name != name {
                return Ok(ControlFlow::Continue(()));
            }
            exact_state = match entry_observation(parent, observed_name)? {
                Some((EntryKind::Directory, identity))
                    if kind == EntryKind::Directory && identity == expected =>
                {
                    BindingState::Exact
                }
                _ => BindingState::Occupied,
            };
            Ok(ControlFlow::Break(()))
        },
    )?;
    if completion == VisitCompletion::LimitExceeded {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(exact_state)
}

#[cfg(target_os = "linux")]
fn exact_directory_binding_state_preallocated(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
    buffer: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<BindingState> {
    for _ in 0..EXACT_DIRECTORY_BINDING_VALIDATION_ATTEMPTS {
        let state = directory_binding_state(parent, name, expected)?;
        if state != BindingState::Exact {
            return Ok(state);
        }
        let revision = directory_revision_preallocated(parent)?;
        let exact_state =
            exact_directory_name_observation_preallocated(parent, name, expected, buffer)?;
        let final_state = directory_binding_state(parent, name, expected)?;
        if final_state != BindingState::Exact {
            return Ok(final_state);
        }
        let final_revision = directory_revision_preallocated(parent)?;
        if exact_state == BindingState::Exact {
            if final_revision == revision {
                return Ok(BindingState::Exact);
            }
            let confirmed =
                exact_directory_name_observation_preallocated(parent, name, expected, buffer)?;
            let confirmed_state = directory_binding_state(parent, name, expected)?;
            if confirmed_state != BindingState::Exact {
                return Ok(confirmed_state);
            }
            if confirmed == BindingState::Exact {
                return Ok(BindingState::Exact);
            }
            if directory_revision_preallocated(parent)? != final_revision {
                continue;
            }
            return Ok(confirmed);
        }
        if final_revision != revision {
            continue;
        }
        return Ok(exact_state);
    }
    Err(io::ErrorKind::WouldBlock.into())
}

pub(crate) fn sync_publication_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        full_fsync(file)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(rfs::fsync(file)?)
    }
}

pub(crate) fn rename_no_replace(
    attempt: &mut PublicationReceipt,
    attempt_id: u64,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &File,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let source_identity = file_identity(source)?;
    if !matches!(attempt.state, PublicationReceiptState::Attempted) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named publication requires an exact unconsumed attempt",
        ));
    }
    PublicationReceipt::validate_binding(
        attempt.binding(),
        attempt_id,
        source_identity,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )?;
    if file_binding_state(source_parent, source_name, source_identity)? != BindingState::Exact {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged file binding changed before promotion",
        ));
    }
    PublicationReceipt::validate_exact_revision(attempt.binding(), source)?;
    rfs::renameat_with(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    attempt.mark_parents_pending();
    PublicationReceipt::validate_content(attempt.binding(), source)?;
    Ok(())
}

pub(crate) fn rename_recovery_publication_no_replace(
    attempt: &mut PublicationReceipt,
    attempt_id: u64,
    parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &File,
    destination_name: &OsStr,
) -> io::Result<()> {
    let source_identity = file_identity(source)?;
    if !matches!(attempt.state, PublicationReceiptState::Attempted) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery publication requires an exact unconsumed attempt",
        ));
    }
    PublicationReceipt::validate_binding(
        attempt.binding(),
        attempt_id,
        source_identity,
        parent,
        source_name,
        parent,
        destination_name,
    )?;
    if file_binding_state(parent, source_name, source_identity)? != BindingState::Exact {
        return Err(binding_changed("recovery publication source changed"));
    }
    PublicationReceipt::validate_exact_revision(attempt.binding(), source)?;
    rfs::renameat_with(
        parent,
        source_name,
        parent,
        destination_name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    attempt.mark_parents_pending();
    Ok(())
}

pub(crate) fn settle_publication(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_file: &File,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let staged_identity = file_identity(staged_file)?;
    let (staged_size, staged_stamp) = file_receipt_fields(staged_file)?;
    settle_publication_observation(
        receipt,
        attempt_id,
        staged_identity,
        staged_size,
        staged_stamp,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        false,
        true,
    )
}

pub(crate) fn settle_publication_deferring_parent_sync(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_file: &File,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let staged_identity = file_identity(staged_file)?;
    let (staged_size, staged_stamp) = file_receipt_fields(staged_file)?;
    settle_publication_observation(
        receipt,
        attempt_id,
        staged_identity,
        staged_size,
        staged_stamp,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        false,
        false,
    )
}

pub(crate) fn settle_recovery_publication(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_file: &File,
    parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_name: &OsStr,
) -> io::Result<()> {
    let staged_identity = file_identity(staged_file)?;
    let (staged_size, staged_stamp) = file_receipt_fields(staged_file)?;
    settle_publication_observation(
        receipt,
        attempt_id,
        staged_identity,
        staged_size,
        staged_stamp,
        parent,
        source_name,
        parent,
        destination_name,
        true,
        true,
    )
}

pub(crate) fn settle_parked_publication(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_file: &FileCleanupHandle,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let staged_identity = parked_file_identity(staged_file)?;
    let (staged_size, staged_stamp) = parked_file_receipt_fields(staged_file)?;
    settle_publication_observation(
        receipt,
        attempt_id,
        staged_identity,
        staged_size,
        staged_stamp,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        false,
        true,
    )
}

fn settle_publication_observation(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_identity: Identity,
    staged_size: u64,
    staged_stamp: FileStamp,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
    retry_poisoned: bool,
    sync_parents: bool,
) -> io::Result<()> {
    PublicationReceipt::validate_content_observation(receipt.binding(), staged_size, staged_stamp)?;
    PublicationReceipt::validate_binding(
        receipt.binding(),
        attempt_id,
        staged_identity,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )?;
    validate_applied_publication(
        staged_identity,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )?;
    if matches!(receipt.state, PublicationReceiptState::Poisoned) && !retry_poisoned {
        return Err(io::Error::other(
            "named publication parent barrier previously failed",
        ));
    }
    if sync_parents && !matches!(receipt.state, PublicationReceiptState::Committed) {
        if let Err(error) = sync_publication_directory(destination_parent) {
            receipt.mark_poisoned();
            return Err(error);
        }
        if directory_identity(source_parent)? != directory_identity(destination_parent)?
            && let Err(error) = sync_publication_directory(source_parent)
        {
            receipt.mark_poisoned();
            return Err(error);
        }
    }
    PublicationReceipt::validate_binding(
        receipt.binding(),
        attempt_id,
        staged_identity,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )?;
    validate_applied_publication(
        staged_identity,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
    )?;
    receipt.mark_committed();
    Ok(())
}

pub(crate) fn sync_publication_directory(directory: &DirectoryHandle) -> io::Result<()> {
    loop {
        #[cfg(test)]
        if let Some(outcome) = publication_directory_sync_test_outcome() {
            match outcome {
                Ok(()) => return Ok(()),
                Err(io::ErrorKind::Interrupted) => continue,
                Err(kind) => {
                    return Err(io::Error::new(
                        kind,
                        "injected publication-directory barrier failure",
                    ));
                }
            }
        }
        #[cfg(target_os = "macos")]
        let result = full_fsync(directory);
        #[cfg(not(target_os = "macos"))]
        let result = rfs::fsync(directory).map_err(io::Error::from);
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
fn full_fsync(handle: &impl AsRawFd) -> io::Result<()> {
    loop {
        let result = unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_FULLFSYNC) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn move_file_no_replace(
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &File,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let source_identity = file_identity(source)?;
    if file_binding_state(source_parent, source_name, source_identity)? != BindingState::Exact {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file binding changed before move",
        ));
    }
    Ok(rfs::renameat_with(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        rfs::RenameFlags::NOREPLACE,
    )?)
}

pub(crate) fn rename_directory_no_replace(
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &DirectoryHandle,
    expected: Identity,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    if directory_identity(source)? != expected
        || directory_binding_state(source_parent, source_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("directory binding changed before move"));
    }
    Ok(rfs::renameat_with(
        source_parent,
        source_name,
        destination_parent,
        destination_name,
        rfs::RenameFlags::NOREPLACE,
    )?)
}

pub(crate) fn park_file_no_replace(
    parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &File,
    expected: Identity,
    park_name: &OsStr,
    cleanup: &FileCleanupHandle,
) -> Result<(), ParkFileError> {
    let admitted = file_identity(source)
        .and_then(|identity| {
            if identity == expected {
                file_binding_state(parent, source_name, expected)
            } else {
                Ok(BindingState::Occupied)
            }
        })
        .map_err(ParkFileError::NoEffect)?;
    if admitted != BindingState::Exact {
        return Err(ParkFileError::NoEffect(io::Error::new(
            io::ErrorKind::InvalidData,
            "file binding changed before parking",
        )));
    }
    if let Err(error) = rfs::renameat_with(
        parent,
        source_name,
        parent,
        park_name,
        rfs::RenameFlags::NOREPLACE,
    ) {
        let error = io::Error::from(error);
        if file_identity(source).ok() == Some(expected)
            && file_identity(&cleanup.0).ok() == Some(expected)
            && file_binding_state(parent, source_name, expected).ok() == Some(BindingState::Exact)
        {
            return Err(ParkFileError::NoEffect(error));
        }
        return Err(ParkFileError::AppliedUnverified(error));
    }
    let settled = sync_directory(parent).and_then(|()| {
        if file_binding_state(parent, source_name, expected)? == BindingState::Absent
            && file_binding_state(parent, park_name, expected)? == BindingState::Exact
        {
            Ok(())
        } else {
            Err(binding_changed("file parking topology was not exact"))
        }
    });
    if let Err(error) = settled {
        return Err(ParkFileError::AppliedUnverified(error));
    }
    if file_identity(&cleanup.0).ok() != Some(expected) {
        return Err(ParkFileError::AppliedUnverified(binding_changed(
            "retained file cleanup authority changed after parking",
        )));
    }
    Ok(())
}

pub(crate) fn open_parked_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    expected: Identity,
) -> io::Result<FileCleanupHandle> {
    let parked = open_file(parent, park_name)?;
    if file_identity(&parked)? != expected {
        return Err(binding_changed("parked file changed before admission"));
    }
    Ok(FileCleanupHandle(parked))
}

pub(crate) fn parked_file_receipt_fields(
    parked: &FileCleanupHandle,
) -> io::Result<(u64, FileStamp)> {
    file_receipt_fields(&parked.0)
}

pub(crate) fn parked_file_identity(parked: &FileCleanupHandle) -> io::Result<Identity> {
    file_identity(&parked.0)
}

pub(crate) fn read_parked_at(
    parked: &FileCleanupHandle,
    bytes: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    read_at(&parked.0, bytes, offset)
}

pub(crate) fn remove_parked_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut FileCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    if file_identity(&parked.0)? != expected
        || file_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "parked file changed before removal",
        ));
    }
    rfs::unlinkat(parent, park_name, AtFlags::empty())?;
    sync_directory(parent)?;
    let (identity, links) = retained_file_identity(&parked.0)?;
    if identity != expected
        || links != 0
        || file_binding_state(parent, park_name, expected)? != BindingState::Absent
    {
        return Err(binding_changed("parked file removal was not exact"));
    }
    Ok(())
}

pub(crate) fn settle_removed_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &FileCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    sync_directory(parent)?;
    let (identity, links) = retained_file_identity(&parked.0)?;
    if identity != expected
        || links != 0
        || file_binding_state(parent, park_name, expected)? != BindingState::Absent
    {
        return Err(binding_changed("parked file removal remains unsettled"));
    }
    Ok(())
}

pub(crate) fn restore_parked_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut FileCleanupHandle,
    expected: Identity,
    original_name: &OsStr,
) -> io::Result<File> {
    if file_identity(&parked.0)? != expected
        || file_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "parked file changed before restoration",
        ));
    }
    rfs::renameat_with(
        parent,
        park_name,
        parent,
        original_name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    sync_directory(parent)?;
    if file_binding_state(parent, park_name, expected)? != BindingState::Absent
        || file_binding_state(parent, original_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("restored file topology was not exact"));
    }
    open_file(parent, original_name)
}

pub(crate) fn settle_restored_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &FileCleanupHandle,
    expected: Identity,
    original_name: &OsStr,
) -> io::Result<File> {
    sync_directory(parent)?;
    if file_identity(&parked.0)? != expected
        || file_binding_state(parent, park_name, expected)? != BindingState::Absent
        || file_binding_state(parent, original_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("file restoration remains unsettled"));
    }
    open_file(parent, original_name)
}

pub(crate) fn park_directory_no_replace(
    parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &DirectoryHandle,
    expected: Identity,
    park_name: &OsStr,
    cleanup: &DirectoryCleanupHandle,
) -> Result<(), ParkDirectoryError> {
    let admitted = directory_identity(source)
        .and_then(|identity| {
            if identity == expected {
                directory_binding_state(parent, source_name, expected)
            } else {
                Ok(BindingState::Occupied)
            }
        })
        .map_err(ParkDirectoryError::NoEffect)?;
    if admitted != BindingState::Exact {
        return Err(ParkDirectoryError::NoEffect(binding_changed(
            "directory changed before parking",
        )));
    }
    if let Err(error) = rfs::renameat_with(
        parent,
        source_name,
        parent,
        park_name,
        rfs::RenameFlags::NOREPLACE,
    ) {
        let error = io::Error::from(error);
        if directory_identity(source).ok() == Some(expected)
            && directory_identity(&cleanup.0).ok() == Some(expected)
            && directory_binding_state(parent, source_name, expected).ok()
                == Some(BindingState::Exact)
        {
            return Err(ParkDirectoryError::NoEffect(error));
        }
        return Err(ParkDirectoryError::AppliedUnverified(error));
    }
    let settled = sync_directory(parent).and_then(|()| {
        if directory_binding_state(parent, source_name, expected)? == BindingState::Absent
            && directory_binding_state(parent, park_name, expected)? == BindingState::Exact
        {
            Ok(())
        } else {
            Err(binding_changed("directory parking topology was not exact"))
        }
    });
    if let Err(error) = settled {
        return Err(ParkDirectoryError::AppliedUnverified(error));
    }
    if directory_identity(&cleanup.0).ok() != Some(expected) {
        return Err(ParkDirectoryError::AppliedUnverified(binding_changed(
            "retained directory cleanup authority changed after parking",
        )));
    }
    Ok(())
}

pub(crate) fn open_parked_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    expected: Identity,
) -> io::Result<DirectoryCleanupHandle> {
    let (parked, identity) = open_directory(parent, park_name)?;
    if identity != expected {
        return Err(binding_changed("parked directory changed before admission"));
    }
    Ok(DirectoryCleanupHandle(parked))
}

pub(crate) fn remove_parked_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut DirectoryCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    if directory_identity(&parked.0)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
        || !entries(&parked.0, 1)?.entries.is_empty()
    {
        return Err(binding_changed(
            "parked directory changed or was not empty before removal",
        ));
    }
    rfs::unlinkat(parent, park_name, AtFlags::REMOVEDIR)?;
    sync_directory(parent)?;
    if directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || !retained_directory_is_removed(&parked.0, expected)?
    {
        return Err(binding_changed("parked directory removal was not exact"));
    }
    Ok(())
}

pub(crate) fn remove_parked_directory_tree(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut DirectoryCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    if directory_identity(&parked.0)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "parked directory tree changed before removal",
        ));
    }
    clear_directory_children(&parked.0, &[])?;
    if directory_identity(&parked.0)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "parked directory tree changed during removal",
        ));
    }
    remove_parked_directory(parent, park_name, parked, expected)
}

pub(crate) fn settle_removed_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &DirectoryCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    sync_directory(parent)?;
    if directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || !retained_directory_is_removed(&parked.0, expected)?
    {
        return Err(binding_changed(
            "parked directory removal remains unsettled",
        ));
    }
    Ok(())
}

pub(crate) fn restore_parked_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut DirectoryCleanupHandle,
    expected: Identity,
    original_name: &OsStr,
) -> io::Result<DirectoryHandle> {
    if directory_identity(&parked.0)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "parked directory changed before restoration",
        ));
    }
    rfs::renameat_with(
        parent,
        park_name,
        parent,
        original_name,
        rfs::RenameFlags::NOREPLACE,
    )?;
    sync_directory(parent)?;
    if directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || directory_binding_state(parent, original_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("restored directory topology was not exact"));
    }
    open_directory(parent, original_name).map(|(handle, _)| handle)
}

pub(crate) fn settle_restored_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &DirectoryCleanupHandle,
    expected: Identity,
    original_name: &OsStr,
) -> io::Result<DirectoryHandle> {
    sync_directory(parent)?;
    if directory_identity(&parked.0)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || directory_binding_state(parent, original_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("directory restoration remains unsettled"));
    }
    open_directory(parent, original_name).map(|(handle, _)| handle)
}

pub(crate) fn sync_directory(directory: &DirectoryHandle) -> io::Result<()> {
    sync_publication_directory(directory)
}

pub(crate) fn try_acquire_lease(root: &RootGuard, name: &OsStr) -> LeaseAcquisitionOutcome {
    if let Err(error) = validate_root(root) {
        return LeaseAcquisitionOutcome::NoEffect(error);
    }
    let name_class_revision = match validate_lease_name_class(root, name, false) {
        Ok(revision) => RwLock::new(revision),
        Err(error) => return LeaseAcquisitionOutcome::NoEffect(error),
    };
    let (handle, created) = match rfs::openat(
        &root.handle,
        name,
        OFlags::RDWR
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    ) {
        Ok(handle) => (File::from(handle), true),
        Err(rustix::io::Errno::EXIST) => match rfs::openat(
            &root.handle,
            name,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(handle) => (File::from(handle), false),
            Err(error) => return LeaseAcquisitionOutcome::NoEffect(error.into()),
        },
        Err(error) => return LeaseAcquisitionOutcome::NoEffect(error.into()),
    };
    if let Err(error) = require_regular_file(&handle) {
        return lease_acquisition_failure(error, handle, created, None, name);
    }
    let identity = match retained_file_identity(&handle) {
        Ok((identity, 1)) => identity,
        Ok(_) => {
            return lease_acquisition_failure(
                binding_changed("application root lease link count changed"),
                handle,
                created,
                None,
                name,
            );
        }
        Err(error) => return lease_acquisition_failure(error, handle, created, None, name),
    };
    if let Err(error) = lock_lease(&handle) {
        return lease_acquisition_failure(error, handle, created, Some(identity), name);
    }
    if let Err(error) = validate_lease_binding(root, name, &handle, identity, &name_class_revision)
    {
        return lease_acquisition_failure(error, handle, created, Some(identity), name);
    }
    if created {
        if let Err(error) = sync_publication_directory(&root.handle) {
            return lease_acquisition_failure(error, handle, true, Some(identity), name);
        }
        if let Err(error) =
            validate_lease_binding(root, name, &handle, identity, &name_class_revision)
        {
            return lease_acquisition_failure(error, handle, true, Some(identity), name);
        }
    }
    let retained_root = match retain_root_proof(root) {
        Ok(root) => root,
        Err(error) => {
            return lease_acquisition_failure(error, handle, created, Some(identity), name);
        }
    };
    LeaseAcquisitionOutcome::Acquired(LeaseHandle {
        handle,
        process: std::process::id(),
        identity,
        root: retained_root,
        name: name.to_os_string(),
        name_class_revision,
    })
}

fn lease_acquisition_failure(
    error: io::Error,
    handle: File,
    created: bool,
    identity: Option<Identity>,
    name: &OsStr,
) -> LeaseAcquisitionOutcome {
    if created {
        LeaseAcquisitionOutcome::AppliedUnverified(LeaseAcquisitionObligation {
            error,
            handle: Some(handle),
            identity,
            name: name.to_os_string(),
            state: LeaseAcquisitionState::Created,
        })
    } else {
        LeaseAcquisitionOutcome::NoEffect(error)
    }
}

fn lock_lease(handle: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::EACCES) | Some(libc::EAGAIN)
    ) {
        Err(io::Error::new(io::ErrorKind::WouldBlock, error))
    } else {
        Err(error)
    }
}

fn validate_lease_binding(
    root: &RootGuard,
    name: &OsStr,
    handle: &File,
    expected: Identity,
    name_class_revision: &RwLock<Option<DirectoryStamp>>,
) -> io::Result<()> {
    validate_root(root)?;
    let identity = file_identity(handle)?;
    if identity != expected
        || file_binding_state(&root.handle, name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("application root lease binding changed"));
    }
    validate_cached_lease_name_class(root, name, expected, name_class_revision)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LeaseNameClassState {
    Absent,
    Exact,
    Invalid,
}

fn observe_lease_name_class(root: &RootGuard, name: &OsStr) -> io::Result<LeaseNameClassState> {
    let mut matching = 0_usize;
    let mut exact = false;
    let completion = visit_entries(
        &root.handle,
        crate::MAX_DIRECTORY_LIST_ENTRIES,
        |candidate, kind| {
            if leaf_names_equal(candidate, name) {
                matching += 1;
                exact = matching == 1 && candidate == name && kind == EntryKind::File;
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    if completion != VisitCompletion::Complete {
        return Err(binding_changed(
            "application root lease enumeration exceeded its bound",
        ));
    }
    Ok(match matching {
        0 => LeaseNameClassState::Absent,
        1 if exact => LeaseNameClassState::Exact,
        _ => LeaseNameClassState::Invalid,
    })
}

fn validate_lease_name_class(
    root: &RootGuard,
    name: &OsStr,
    require_exact: bool,
) -> io::Result<Option<DirectoryStamp>> {
    for attempt in 0..LEASE_NAME_CLASS_VALIDATION_ATTEMPTS {
        let revision = directory_revision(&root.handle)?;
        let state = observe_lease_name_class(root, name)?;
        if directory_revision(&root.handle)? != revision {
            if attempt + 1 == LEASE_NAME_CLASS_VALIDATION_ATTEMPTS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "application root lease name remained unstable during root churn",
                ));
            }
            continue;
        }
        return match state {
            LeaseNameClassState::Exact => Ok(Some(revision)),
            LeaseNameClassState::Absent if !require_exact => Ok(None),
            LeaseNameClassState::Absent | LeaseNameClassState::Invalid => Err(binding_changed(
                "application root lease has an aliased or noncanonical name",
            )),
        };
    }
    unreachable!("lease name-class retry loop has a non-zero bound")
}

fn validate_cached_lease_name_class(
    root: &RootGuard,
    name: &OsStr,
    expected: Identity,
    cached_revision: &RwLock<Option<DirectoryStamp>>,
) -> io::Result<()> {
    let observed_revision = directory_revision(&root.handle)?;
    // This is the same kernel-revision trust boundary as exact directory binding:
    // cooperative namespace mutations invalidate the proof before another scan is skipped.
    if cached_revision
        .read()
        .map_err(|_| io::Error::other("lease name-class proof lock is poisoned"))?
        .as_ref()
        == Some(&observed_revision)
    {
        return Ok(());
    }
    let mut cached_revision = cached_revision
        .write()
        .map_err(|_| io::Error::other("lease name-class proof lock is poisoned"))?;
    for attempt in 0..LEASE_NAME_CLASS_VALIDATION_ATTEMPTS {
        if file_binding_state(&root.handle, name, expected)? != BindingState::Exact {
            return Err(binding_changed("application root lease binding changed"));
        }
        let revision = directory_revision(&root.handle)?;
        if cached_revision.as_ref() == Some(&revision) {
            return Ok(());
        }
        *cached_revision = None;
        let state = observe_lease_name_class(root, name)?;
        if file_binding_state(&root.handle, name, expected)? != BindingState::Exact {
            return Err(binding_changed("application root lease binding changed"));
        }
        if directory_revision(&root.handle)? != revision {
            if attempt + 1 == LEASE_NAME_CLASS_VALIDATION_ATTEMPTS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "application root lease name remained unstable during root churn",
                ));
            }
            continue;
        }
        if state != LeaseNameClassState::Exact {
            return Err(binding_changed(
                "application root lease has an aliased or noncanonical name",
            ));
        }
        *cached_revision = Some(revision);
        return Ok(());
    }
    unreachable!("lease name-class retry loop has a non-zero bound")
}

pub(crate) fn reconcile_lease_acquisition(
    root: &RootGuard,
    mut obligation: LeaseAcquisitionObligation,
) -> Result<LeaseHandle, LeaseAcquisitionObligation> {
    if !matches!(obligation.state, LeaseAcquisitionState::Created) {
        obligation.error = io::Error::other("removed lease acquisition cannot be reconciled");
        return Err(obligation);
    }
    let Some(handle) = obligation.handle.as_ref() else {
        obligation.error = io::Error::other("lease acquisition lost its retained file");
        return Err(obligation);
    };
    let identity = match retained_file_identity(handle) {
        Ok((identity, 1)) => identity,
        Ok(_) => {
            obligation.error = binding_changed("application root lease link count changed");
            return Err(obligation);
        }
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    if obligation
        .identity
        .is_some_and(|expected| expected != identity)
    {
        obligation.error = binding_changed("lease acquisition handle changed identity");
        return Err(obligation);
    }
    obligation.identity = Some(identity);
    let name_class_revision = RwLock::new(None);
    if let Err(error) = lock_lease(handle)
        .and_then(|()| {
            validate_lease_binding(
                root,
                &obligation.name,
                handle,
                identity,
                &name_class_revision,
            )
        })
        .and_then(|()| sync_publication_directory(&root.handle))
        .and_then(|()| {
            validate_lease_binding(
                root,
                &obligation.name,
                handle,
                identity,
                &name_class_revision,
            )
        })
    {
        obligation.error = error;
        return Err(obligation);
    }
    let retained_root = match retain_root_proof(root) {
        Ok(root) => root,
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    Ok(LeaseHandle {
        handle: obligation
            .handle
            .take()
            .expect("reconciled lease acquisition retains its file"),
        process: std::process::id(),
        identity,
        root: retained_root,
        name: obligation.name.clone(),
        name_class_revision,
    })
}

pub(crate) fn cleanup_lease_acquisition(
    root: &RootGuard,
    name: &OsStr,
    mut obligation: LeaseAcquisitionObligation,
) -> Result<(), LeaseAcquisitionObligation> {
    if obligation.name != name {
        obligation.error = binding_changed("lease acquisition cleanup name changed");
        return Err(obligation);
    }
    if let Err(error) = validate_root(root) {
        obligation.error = error;
        return Err(obligation);
    }
    let Some(handle) = obligation.handle.as_ref() else {
        obligation.error = io::Error::other("lease acquisition cleanup lost its retained file");
        return Err(obligation);
    };
    let (identity, links) = match retained_file_identity(handle) {
        Ok(observation) => observation,
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    if obligation
        .identity
        .is_some_and(|expected| expected != identity)
    {
        obligation.error = binding_changed("lease acquisition cleanup identity changed");
        return Err(obligation);
    }
    obligation.identity = Some(identity);
    if matches!(obligation.state, LeaseAcquisitionState::Created) {
        let binding = match file_binding_state(&root.handle, name, identity) {
            Ok(binding) => binding,
            Err(error) => {
                obligation.error = error;
                return Err(obligation);
            }
        };
        if links == 0 && binding == BindingState::Absent {
            obligation.state = LeaseAcquisitionState::Removed;
        } else if links == 1 && binding == BindingState::Exact {
            if let Err(error) = rfs::unlinkat(&root.handle, name, AtFlags::empty()) {
                obligation.error = error.into();
                return Err(obligation);
            }
            obligation.state = LeaseAcquisitionState::Removed;
        } else {
            obligation.error = binding_changed("created lease cleanup binding changed");
            return Err(obligation);
        }
    }
    let settled = retained_file_identity(handle).and_then(|(retained, retained_links)| {
        if retained != identity
            || retained_links != 0
            || file_binding_state(&root.handle, name, identity)? != BindingState::Absent
        {
            return Err(binding_changed("created lease cleanup removal changed"));
        }
        sync_publication_directory(&root.handle)?;
        validate_root(root)?;
        if file_binding_state(&root.handle, name, identity)? != BindingState::Absent {
            return Err(binding_changed("created lease cleanup did not settle"));
        }
        Ok(())
    });
    match settled {
        Ok(()) => Ok(()),
        Err(error) => {
            obligation.error = error;
            Err(obligation)
        }
    }
}

pub(crate) fn lease_acquisition_error(obligation: &LeaseAcquisitionObligation) -> &io::Error {
    &obligation.error
}

pub(crate) fn validate_lease(lease: &LeaseHandle) -> io::Result<()> {
    validate_lease_binding(
        &lease.root,
        &lease.name,
        &lease.handle,
        lease.identity,
        &lease.name_class_revision,
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_lease_preallocated(
    lease: &LeaseHandle,
    buffer: &mut [std::mem::MaybeUninit<u8>],
) -> io::Result<()> {
    validate_root_preallocated(&lease.root, buffer)?;
    let (identity, links) = retained_file_identity_preallocated(&lease.handle)?;
    if identity == lease.identity
        && links == 1
        && file_binding_state(&lease.root.handle, &lease.name, lease.identity)?
            == BindingState::Exact
    {
        Ok(())
    } else {
        Err(io::ErrorKind::InvalidData.into())
    }
}

pub(crate) fn recovery_control_initialize_len(lease: &LeaseHandle) -> io::Result<()> {
    validate_lease(lease)?;
    let length = lease.handle.metadata()?.len();
    if length == 0 {
        lease.handle.set_len(RECOVERY_CONTROL_BYTES)?;
        sync_recovery_control_file(&lease.handle)?;
    } else if length != RECOVERY_CONTROL_BYTES {
        return Err(binding_changed("recovery control length is corrupt"));
    }
    validate_recovery_control(lease)?;
    Ok(())
}

pub(crate) fn recovery_control_read_exact_at(
    lease: &LeaseHandle,
    offset: u64,
    bytes: &mut [u8],
) -> io::Result<()> {
    validate_recovery_control_range(lease, offset, bytes.len())?;
    recovery_control_read_exact_with(offset, bytes, |bytes, position| {
        lease.handle.read_at(bytes, position)
    })?;
    validate_recovery_control(lease)
}

pub(crate) fn recovery_control_write_all_at(
    lease: &LeaseHandle,
    offset: u64,
    bytes: &[u8],
) -> io::Result<()> {
    validate_recovery_control_range(lease, offset, bytes.len())?;
    recovery_control_write_all_with(offset, bytes, |bytes, position| {
        lease.handle.write_at(bytes, position)
    })?;
    validate_recovery_control(lease)
}

pub(crate) fn recovery_control_sync(lease: &LeaseHandle) -> Result<(), RecoveryControlSyncError> {
    validate_recovery_control(lease).map_err(RecoveryControlSyncError::before_barrier)?;
    sync_recovery_control_file(&lease.handle).map_err(RecoveryControlSyncError::before_barrier)?;
    validate_recovery_control(lease).map_err(RecoveryControlSyncError::after_barrier)
}

fn sync_recovery_control_file(handle: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    return full_fsync(handle);
    #[cfg(not(target_os = "macos"))]
    loop {
        match rfs::fsync(handle) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn validate_recovery_control_range(
    lease: &LeaseHandle,
    offset: u64,
    length: usize,
) -> io::Result<()> {
    validate_recovery_control(lease)?;
    let end = offset
        .checked_add(u64::try_from(length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery control length is not representable",
            )
        })?)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery control range overflowed",
            )
        })?;
    if end > RECOVERY_CONTROL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery control range exceeds fixed capacity",
        ));
    }
    Ok(())
}

fn validate_recovery_control(lease: &LeaseHandle) -> io::Result<()> {
    validate_lease(lease)?;
    if lease.handle.metadata()?.len() != RECOVERY_CONTROL_BYTES {
        return Err(binding_changed("recovery control length is not exact"));
    }
    Ok(())
}

fn require_regular_file(handle: &impl std::os::fd::AsFd) -> io::Result<rfs::Stat> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem capability is not an exact single-link regular file",
        ));
    }
    Ok(stat)
}

fn retained_file_identity(handle: &impl std::os::fd::AsFd) -> io::Result<(Identity, u64)> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retained cleanup capability is not a regular file",
        ));
    }
    let links = normalize_link_count(stat.st_nlink);
    Ok((identity_from_stat(stat), links))
}

#[cfg(target_os = "linux")]
fn retained_file_identity_preallocated(
    handle: &impl std::os::fd::AsFd,
) -> io::Result<(Identity, u64)> {
    let stat = rfs::fstat(handle)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let links = normalize_link_count(stat.st_nlink);
    Ok((identity_from_stat(stat), links))
}

fn normalize_link_count<T: Into<u64>>(links: T) -> u64 {
    links.into()
}

fn binding_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
