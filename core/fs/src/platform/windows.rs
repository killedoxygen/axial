use super::*;
use std::mem::{MaybeUninit, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::FileExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, PathBuf, Prefix};
use std::sync::RwLock;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_WRITE_ATTRIBUTES, FileBasicInfo, FileDispositionInfoEx, FileIdBothDirectoryInfo,
    FileIdBothDirectoryRestartInfo, FileIdInfo, FileNameInfo, FileStandardInfo,
    GetFileInformationByHandleEx, GetVolumeInformationByHandleW, SetFileInformationByHandle,
};

const DELETE_ACCESS: u32 = 0x0001_0000;
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const FILE_READ_DATA_ACCESS: u32 = 0x0001;
const FILE_WRITE_DATA_ACCESS: u32 = 0x0002;
const FILE_TRAVERSE_ACCESS: u32 = 0x0020;
const ERROR_NO_MORE_FILES: i32 = 18;
const ERROR_SHARING_VIOLATION: i32 = 32;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
pub(crate) const MAX_TREE_CLEAR_DEPTH: usize = 128;
const MAX_TREE_CLEAR_ENTRIES: usize = 1_000_000;
const LEASE_NAME_CLASS_VALIDATION_ATTEMPTS: usize = 4;

#[cfg(test)]
thread_local! {
    static RECOVERY_CONTROL_READ_TEST_LENGTH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct RecoveryControlReadTestHookGuard {
    thread: std::thread::ThreadId,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
impl Drop for RecoveryControlReadTestHookGuard {
    fn drop(&mut self) {
        assert_eq!(
            self.thread,
            std::thread::current().id(),
            "recovery-control read test hook guard changed threads"
        );
        RECOVERY_CONTROL_READ_TEST_LENGTH.set(None);
    }
}

#[cfg(test)]
pub(crate) fn install_recovery_control_read_test_hook(
    length: u64,
) -> RecoveryControlReadTestHookGuard {
    assert!(
        RECOVERY_CONTROL_READ_TEST_LENGTH
            .replace(Some(length))
            .is_none(),
        "recovery-control read test hook is already installed"
    );
    RecoveryControlReadTestHookGuard {
        thread: std::thread::current().id(),
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(test)]
fn run_recovery_control_read_test_hook(lease: &LeaseHandle) -> io::Result<()> {
    if let Some(length) = RECOVERY_CONTROL_READ_TEST_LENGTH.take() {
        lease.handle.set_len(length)?;
    }
    Ok(())
}

struct NtOpenResult {
    handle: File,
    information: usize,
}

pub(crate) struct DirectoryHandle {
    file: File,
    enumeration: std::sync::Arc<std::sync::Mutex<()>>,
}

impl DirectoryHandle {
    fn new(file: File) -> Self {
        Self {
            file,
            enumeration: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    fn try_clone_shared_cursor(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            enumeration: std::sync::Arc::clone(&self.enumeration),
        })
    }
}

impl std::ops::Deref for DirectoryHandle {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

pub(crate) struct FileCleanupHandle {
    observation: File,
    deletion: Option<File>,
}

pub(crate) struct DirectoryCleanupHandle {
    observation: DirectoryHandle,
    deletion: Option<File>,
}

pub(crate) struct ProcessImageAncestry {
    image: File,
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
}

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
}

pub(crate) struct RootConstruction {
    target: PathBuf,
    guard: Option<RootGuard>,
    created: Vec<RootCreatedBinding>,
    unclassified: Vec<RootCreationReservation>,
}

pub(crate) struct RootCreatedBinding {
    parent: DirectoryHandle,
    name: OsString,
    identity: Identity,
    child: DirectoryHandle,
    deletion: Option<File>,
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

pub(crate) enum LeaseAcquisitionOutcome {
    Acquired(LeaseHandle),
    NoEffect(io::Error),
    AppliedUnverified(LeaseAcquisitionObligation),
}

pub(crate) struct LeaseAcquisitionObligation {
    error: io::Error,
    handle: Option<File>,
    name: OsString,
    state: LeaseAcquisitionState,
}

enum LeaseAcquisitionState {
    Created { identity: Option<Identity> },
    Opened { identity: Option<Identity> },
    Unclassified { identity: Option<Identity> },
    DeletionAdmitted { identity: Identity },
}

impl LeaseAcquisitionState {
    fn identity(&self) -> Option<Identity> {
        match self {
            Self::Created { identity }
            | Self::Opened { identity }
            | Self::Unclassified { identity } => *identity,
            Self::DeletionAdmitted { identity } => Some(*identity),
        }
    }

    fn retain_identity(&mut self, retained: Identity) {
        match self {
            Self::Created { identity }
            | Self::Opened { identity }
            | Self::Unclassified { identity } => *identity = Some(retained),
            Self::DeletionAdmitted { identity } => *identity = retained,
        }
    }

    fn is_created(&self) -> bool {
        matches!(self, Self::Created { .. } | Self::DeletionAdmitted { .. })
    }
}

pub(crate) struct LeaseHandle {
    handle: File,
    identity: Identity,
    root: RootGuard,
    name: OsString,
    name_class_revision: RwLock<Option<DirectoryStamp>>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Identity {
    volume: u64,
    id: [u8; 16],
}

pub(crate) fn identity_witness(identity: Identity) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"axial-fs-windows-identity-v1");
    digest.update(identity.volume.to_le_bytes());
    digest.update(identity.id);
    digest.finalize().into()
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct FileStamp {
    modified: i64,
    changed: i64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct DirectoryStamp {
    modified: i64,
    changed: i64,
}

const WINDOWS_TO_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

pub(crate) fn leaf_names_equal_native(first: &OsStr, second: &OsStr) -> bool {
    use ntapi::ntrtl::RtlEqualUnicodeString;
    use ntapi::winapi::shared::ntdef::UNICODE_STRING;

    let mut first = match encode_leaf(first) {
        Ok(encoded) => encoded,
        Err(_) => return false,
    };
    let mut second = match encode_leaf(second) {
        Ok(encoded) => encoded,
        Err(_) => return false,
    };
    let Some(first_length) = first
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
    else {
        return false;
    };
    let Some(second_length) = second
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
    else {
        return false;
    };
    let first = UNICODE_STRING {
        Length: first_length,
        MaximumLength: first_length,
        Buffer: first.as_mut_ptr(),
    };
    let second = UNICODE_STRING {
        Length: second_length,
        MaximumLength: second_length,
        Buffer: second.as_mut_ptr(),
    };
    unsafe { RtlEqualUnicodeString(&first, &second, 1) != 0 }
}

pub(crate) fn leaf_name_native_key(name: &OsStr) -> Vec<u8> {
    use ntapi::ntrtl::RtlUpcaseUnicodeChar;
    use std::os::windows::ffi::OsStrExt;

    let mut key = vec![b'n'];
    for unit in name.encode_wide() {
        let upper = unsafe { RtlUpcaseUnicodeChar(unit) };
        key.extend_from_slice(&upper.to_le_bytes());
    }
    key
}

pub(crate) fn fill_leaf_name_native_key(name: &OsStr, key: &mut Vec<u8>) -> io::Result<()> {
    use ntapi::ntrtl::RtlUpcaseUnicodeChar;
    use std::os::windows::ffi::OsStrExt;

    for unit in name.encode_wide() {
        let upper = unsafe { RtlUpcaseUnicodeChar(unit) };
        extend_preallocated_key(key, &upper.to_le_bytes())?;
    }
    Ok(())
}

pub(crate) fn capture_process_image_ancestry(path: &Path) -> io::Result<ProcessImageAncestry> {
    let (anchor, names) = windows_absolute_components(path)?;
    let (image_name, directory_names) = names
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "executable has no leaf"))?;
    let (mut current, _) = open_root_anchor(&anchor)?;
    let mut bindings = Vec::new();
    for name in directory_names {
        let child =
            open_root_chain_directory_with_attributes(&current, name, OBJ_CASE_INSENSITIVE)?;
        let identity = directory_identity(&child)?;
        bindings.push(ProcessImageBinding {
            parent: current,
            name: name.clone(),
            identity,
            kind: EntryKind::Directory,
        });
        current = child;
    }
    let image = nt_open_relative_with_attributes(
        &current,
        image_name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OBJ_CASE_INSENSITIVE,
    )?;
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
        if !process_image_binding_matches(binding)? {
            return Err(binding_changed("process image ancestry changed binding"));
        }
    }
    Ok(())
}

fn process_image_binding_matches(binding: &ProcessImageBinding) -> io::Result<bool> {
    let observed = match binding.kind {
        EntryKind::Directory => {
            let handle = open_root_chain_directory_with_attributes(
                &binding.parent,
                &binding.name,
                OBJ_CASE_INSENSITIVE,
            )?;
            directory_identity(&handle)?
        }
        EntryKind::File => {
            let handle = nt_open_relative_with_attributes(
                &binding.parent,
                &binding.name,
                FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
                ntapi::ntioapi::FILE_OPEN,
                ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
                    | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
                    | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                OBJ_CASE_INSENSITIVE,
            )?;
            process_image_identity(&handle)?
        }
        EntryKind::Link | EntryKind::Other => return Ok(false),
    };
    Ok(observed == binding.identity)
}

fn process_image_identity(image: &File) -> io::Result<Identity> {
    let basic = query_basic(image)?;
    let standard = query_standard(image)?;
    if basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
        || standard.Directory
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process image is not an exact regular file",
        ));
    }
    object_identity(image)
}

pub(crate) fn open_absolute_directory_guard(path: &Path) -> io::Result<AbsoluteDirectoryGuard> {
    let (anchor, names) = windows_absolute_components(path)?;
    let (mut current, _) = open_root_anchor(&anchor)?;
    let mut bindings = Vec::new();
    for name in names {
        let child =
            open_root_chain_directory_with_attributes(&current, &name, OBJ_CASE_INSENSITIVE)?;
        let identity = directory_identity(&child)?;
        bindings.push(AbsoluteDirectoryBinding {
            parent: current,
            name,
            identity,
            exact_name: true,
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
            root_chain_exact_binding_state(&binding.parent, &binding.name, binding.identity)?
        } else {
            root_chain_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(binding_changed(
                "external directory ancestry changed binding",
            ));
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

fn windows_absolute_components(path: &Path) -> io::Result<(PathBuf, Vec<OsString>)> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute path has no volume",
        ));
    };
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _)
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute path uses an unsupported Windows namespace",
        ));
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "absolute path contains an unsupported component",
                ));
            }
        }
    }
    let mut anchor = PathBuf::from(prefix.as_os_str());
    anchor.push(std::path::MAIN_SEPARATOR_STR);
    Ok((anchor, names))
}

#[expect(
    clippy::result_large_err,
    reason = "the linear root-construction failure is immediately unpacked for retry or cleanup and must retain its exact handles"
)]
pub(crate) fn open_or_create_root(path: &Path) -> Result<RootConstruction, RootConstructionError> {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(RootConstructionError::without_effect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application root has no volume",
        )));
    };
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::UNC(_, _) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _)
    ) {
        return Err(RootConstructionError::without_effect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application root uses an unsupported Windows namespace",
        )));
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(RootConstructionError::without_effect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "application root is not absolute",
        )));
    }
    let mut anchor = PathBuf::from(prefix.as_os_str());
    anchor.push(std::path::MAIN_SEPARATOR_STR);
    let (mut current, _) =
        open_root_anchor(&anchor).map_err(RootConstructionError::without_effect)?;
    let mut bindings = Vec::new();
    let mut created = Vec::new();
    let mut unclassified = Vec::new();
    for component in components {
        let Component::Normal(name) = component else {
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
        let child = match open_root_chain_directory(&current, name) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if created.try_reserve(1).is_err() || unclassified.try_reserve(1).is_err() {
                    return Err(root_construction_error(
                        io::Error::other("could not reserve root construction recovery capacity"),
                        path,
                        created,
                        unclassified,
                    ));
                }
                let retained_name = name.to_os_string();
                let parent = match clone_directory_handle(&current) {
                    Ok(parent) => parent,
                    Err(error) => {
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                };
                match create_root_chain_directory(&current, name) {
                    Ok(creator) => {
                        let identity = match directory_identity(&creator) {
                            Ok(identity) => identity,
                            Err(error) => {
                                unclassified.push(RootCreationReservation {
                                    parent,
                                    name: retained_name,
                                    child: Some(creator),
                                    published: true,
                                });
                                return Err(root_construction_error(
                                    error,
                                    path,
                                    created,
                                    unclassified,
                                ));
                            }
                        };
                        created.push(RootCreatedBinding {
                            parent,
                            name: retained_name,
                            identity,
                            child: creator,
                            deletion: None,
                            published: true,
                        });
                        let observation = match open_root_chain_directory(&current, name) {
                            Ok(observation) => observation,
                            Err(error) => {
                                return Err(root_construction_error(
                                    error,
                                    path,
                                    created,
                                    unclassified,
                                ));
                            }
                        };
                        match directory_identity(&observation) {
                            Ok(observed) if observed == identity => {}
                            Ok(_) => {
                                return Err(root_construction_error(
                                    binding_changed(
                                        "created root directory changed before observation",
                                    ),
                                    path,
                                    created,
                                    unclassified,
                                ));
                            }
                            Err(error) => {
                                return Err(root_construction_error(
                                    error,
                                    path,
                                    created,
                                    unclassified,
                                ));
                            }
                        }
                        let cleanup_observation = match clone_directory_handle(&observation) {
                            Ok(observation) => observation,
                            Err(error) => {
                                return Err(root_construction_error(
                                    error,
                                    path,
                                    created,
                                    unclassified,
                                ));
                            }
                        };
                        let creator = std::mem::replace(
                            &mut created
                                .last_mut()
                                .expect("created root binding is retained")
                                .child,
                            cleanup_observation,
                        );
                        let DirectoryHandle {
                            file: creator,
                            enumeration: _,
                        } = creator;
                        created
                            .last_mut()
                            .expect("created root binding is retained")
                            .deletion = Some(creator);
                        observation
                    }
                    Err(CreateDirectoryError::NoEffect(error)) => {
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                    Err(CreateDirectoryError::CreatedUnclassified(error)) => {
                        unclassified.push(RootCreationReservation {
                            parent,
                            name: retained_name,
                            child: None,
                            published: true,
                        });
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                    Err(CreateDirectoryError::AppliedUnverified { error, retained }) => {
                        match directory_identity(&retained) {
                            Ok(identity) => created.push(RootCreatedBinding {
                                parent,
                                name: retained_name,
                                identity,
                                child: retained,
                                deletion: None,
                                published: true,
                            }),
                            Err(_) => unclassified.push(RootCreationReservation {
                                parent,
                                name: retained_name,
                                child: Some(retained),
                                published: true,
                            }),
                        }
                        return Err(root_construction_error(error, path, created, unclassified));
                    }
                }
            }
            Err(error) => {
                return Err(root_construction_error(error, path, created, unclassified));
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
                "application root cannot be a volume root",
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

fn clone_directory_handle(directory: &DirectoryHandle) -> io::Result<DirectoryHandle> {
    let handle = directory.try_clone_shared_cursor()?;
    require_directory(&handle)?;
    Ok(handle)
}

pub(crate) fn named_stage_evidence(file: &File) -> io::Result<(Identity, u64)> {
    retained_file_identity(file).map(|(identity, links)| (identity, u64::from(links)))
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

#[expect(
    clippy::result_large_err,
    reason = "root reconciliation returns its exact linear construction directly to the bounded caller retry loop"
)]
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
        match root_chain_binding_state(&binding.parent, &binding.name, binding.identity) {
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
    let mut unpublished = Vec::new();
    for binding in std::mem::take(&mut construction.created) {
        if binding.published {
            construction.created.push(binding);
        } else {
            unpublished.push(binding);
        }
    }
    if !unpublished.is_empty() {
        let debris = RootConstruction {
            target: construction.target.clone(),
            guard: None,
            created: unpublished,
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

#[expect(
    clippy::result_large_err,
    reason = "root cleanup failure must return its exact linear construction for the bounded caller retry loop"
)]
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
            if retained_directory_is_removed(&binding.child, binding.identity)? {
                sync_directory(&binding.parent)?;
                return if retained_directory_is_removed(&binding.child, binding.identity)? {
                    Ok(())
                } else {
                    Err(binding_changed(
                        "created root cleanup removal proof did not remain stable",
                    ))
                };
            }
            if directory_identity(&binding.child)? != binding.identity
                || !entries(&binding.child, 1)?.entries.is_empty()
            {
                return Err(binding_changed(
                    "created root directory changed before cleanup",
                ));
            }
            if binding.deletion.is_none() {
                binding.deletion = Some(binding.child.try_clone()?);
            }
            let deletion = binding
                .deletion
                .as_ref()
                .expect("root cleanup retains deletion authority");
            require_directory(deletion)?;
            if object_identity(deletion)? != binding.identity {
                return Err(binding_changed(
                    "root cleanup deleter does not retain the created directory",
                ));
            }
            let deletion_observation = DirectoryHandle::new(deletion.try_clone()?);
            if !entries(&deletion_observation, 1)?.entries.is_empty() {
                return Err(binding_changed(
                    "root cleanup deleter observed a non-empty directory",
                ));
            }
            match root_chain_binding_state(&binding.parent, &binding.name, binding.identity)? {
                BindingState::Absent => {
                    set_delete(deletion)?;
                    let standard = query_standard(deletion)?;
                    if !standard.DeletePending && standard.NumberOfLinks != 0 {
                        return Err(binding_changed(
                            "root cleanup deleter did not retain deletion state",
                        ));
                    }
                    drop(binding.deletion.take());
                    sync_directory(&binding.parent)?;
                    return if retained_directory_is_removed(&binding.child, binding.identity)? {
                        Ok(())
                    } else {
                        Err(binding_changed(
                            "created root directory cleanup was not exact",
                        ))
                    };
                }
                BindingState::Exact | BindingState::Occupied => {}
            }
            set_delete(deletion)?;
            let standard = query_standard(deletion)?;
            if !standard.DeletePending && standard.NumberOfLinks != 0 {
                return Err(binding_changed(
                    "root cleanup deleter did not retain deletion state",
                ));
            }
            drop(binding.deletion.take());
            sync_directory(&binding.parent)?;
            if !retained_directory_is_removed(&binding.child, binding.identity)? {
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

fn retained_directory_is_removed(child: &DirectoryHandle, expected: Identity) -> io::Result<bool> {
    let standard = query_standard(child)?;
    Ok(object_identity(child)? == expected && standard.NumberOfLinks == 0)
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
    if let (Some(identity), Some(child)) = (identity, creation.child.as_ref()) {
        match retained_directory_is_removed(child, identity) {
            Ok(true) => return Ok(None),
            Ok(false) => {}
            Err(error) => return Err((error, creation)),
        }
    }
    let state = match identity {
        Some(identity) => {
            match root_chain_binding_state(&creation.parent, &creation.name, identity) {
                Ok(state) => state,
                Err(error) => return Err((error, creation)),
            }
        }
        None => match root_chain_directory_identity(&creation.parent, &creation.name) {
            Ok(None) => BindingState::Absent,
            Ok(Some(_)) => BindingState::Occupied,
            Err(error) => return Err((error, creation)),
        },
    };
    match identity {
        Some(identity) => {
            let child = creation
                .child
                .take()
                .expect("classified root creation retains its child");
            Ok(Some(RootCreatedBinding {
                parent: creation.parent,
                name: creation.name,
                identity,
                child,
                deletion: None,
                published: creation.published && state == BindingState::Exact,
            }))
        }
        None => Err((
            binding_changed("unclassified root creation could not be proven exact"),
            creation,
        )),
    }
}

fn open_root_anchor(path: &Path) -> io::Result<(DirectoryHandle, Identity)> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE_ACCESS | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let handle = DirectoryHandle::new(options.open(path)?);
    require_directory(&handle)?;
    let identity = directory_identity(&handle)?;
    Ok((handle, identity))
}

pub(crate) fn clone_root(root: &RootGuard) -> io::Result<DirectoryHandle> {
    validate_root_handle(root)?;
    let handle = clone_directory_handle(&root.handle)?;
    if directory_identity(&handle)? != root.identity {
        return Err(binding_changed(
            "application root changed before capability mint",
        ));
    }
    Ok(handle)
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
            root_chain_exact_binding_state(&binding.parent, &binding.name, binding.identity)?
        } else {
            root_chain_binding_state(&binding.parent, &binding.name, binding.identity)?
        };
        if state != BindingState::Exact {
            return Err(binding_changed("application root ancestry changed binding"));
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
    validate_windows_lease_binding(
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
                    return Err(binding_changed("preserved directory tree entry changed"));
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
                let retained = nt_open_relative(
                    &frame.directory,
                    &name,
                    FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
                    ntapi::ntioapi::FILE_OPEN,
                    ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
                        | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                )?;
                if object_identity(&retained)? != observed_identity {
                    return Err(binding_changed(
                        "directory tree entry changed before admission",
                    ));
                }
                remove_tree_entry(&frame.directory, &name, &retained, observed_identity)?;
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
        remove_tree_entry(parent, &name, &completed.directory, identity)?;
    }
    Ok(())
}

fn remove_tree_entry(
    parent: &DirectoryHandle,
    name: &OsStr,
    retained: &File,
    expected: Identity,
) -> io::Result<()> {
    if object_identity(retained)? != expected {
        return Err(binding_changed(
            "directory tree entry changed before deletion",
        ));
    }
    let deletion = nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | DELETE_ACCESS | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_OPEN_REPARSE_POINT | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    if object_identity(&deletion)? != expected {
        return Err(binding_changed(
            "directory tree entry changed before deletion",
        ));
    }
    set_delete(&deletion)?;
    drop(deletion);
    sync_directory(parent)?;
    let standard = query_standard(retained)?;
    if object_identity(retained)? != expected
        || standard.NumberOfLinks != 0
        || entry_observation(parent, name)?.is_some()
    {
        return Err(binding_changed(
            "directory tree entry deletion was not exact",
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

pub(crate) fn directory_identity(handle: &DirectoryHandle) -> io::Result<Identity> {
    require_directory(handle)?;
    object_identity(handle)
}

pub(crate) fn directory_revision(handle: &DirectoryHandle) -> io::Result<DirectoryStamp> {
    require_directory(handle)?;
    let basic = query_basic(handle)?;
    Ok(DirectoryStamp {
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    })
}

pub(crate) fn directory_created_at_ns(handle: &DirectoryHandle) -> io::Result<u64> {
    require_directory(handle)?;
    let created = query_basic(handle)?.CreationTime;
    if created == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem birth time is unavailable",
        ));
    }
    let ticks = u64::try_from(created)
        .ok()
        .and_then(|created| created.checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "birth time precedes the Unix epoch",
            )
        })?;
    birth_time_ns(
        (ticks / 10_000_000) as i64,
        ((ticks % 10_000_000) * 100) as i64,
    )
}

pub(crate) fn open_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<(DirectoryHandle, Identity)> {
    let handle = DirectoryHandle::new(nt_open_relative(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?);
    require_directory(&handle)?;
    let identity = object_identity(&handle)?;
    Ok((handle, identity))
}

fn open_root_chain_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<DirectoryHandle> {
    open_root_chain_directory_with_attributes(parent, name, OBJ_CASE_INSENSITIVE)
}

fn open_root_chain_directory_with_attributes(
    parent: &DirectoryHandle,
    name: &OsStr,
    object_flags: u32,
) -> io::Result<DirectoryHandle> {
    let handle = DirectoryHandle::new(nt_open_relative_with_attributes(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        object_flags,
    )?);
    require_directory(&handle)?;
    Ok(handle)
}

fn root_chain_directory_identity(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<Option<Identity>> {
    match open_root_chain_directory(parent, name) {
        Ok(handle) => Ok(Some(directory_identity(&handle)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn root_chain_binding_state(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<BindingState> {
    match open_root_chain_directory(parent, name) {
        Ok(handle) => Ok(if directory_identity(&handle)? == expected {
            BindingState::Exact
        } else {
            BindingState::Occupied
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(BindingState::Absent),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(BindingState::Occupied),
        Err(error) => Err(error),
    }
}

fn root_chain_exact_binding_state(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<BindingState> {
    let handle = match open_root_chain_directory(parent, name) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BindingState::Absent);
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return Ok(BindingState::Occupied);
        }
        Err(error) => return Err(error),
    };
    if directory_identity(&handle)? != expected {
        return Ok(BindingState::Occupied);
    }
    if opened_directory_leaf_name(&handle)?.as_os_str() == name {
        Ok(BindingState::Exact)
    } else {
        Ok(BindingState::Occupied)
    }
}

fn opened_directory_leaf_name(directory: &DirectoryHandle) -> io::Result<OsString> {
    opened_file_leaf_name(&directory.file)
}

fn opened_file_path(file: &File) -> io::Result<PathBuf> {
    // Most paths fit the stack buffer; any refusal retries with the full bound.
    let mut short = [0_u64; 2 * 1024 / size_of::<u64>()];
    if let Ok(path) = opened_file_path_in(file, &mut short) {
        return Ok(path);
    }
    const BUFFER_BYTES: usize = 128 * 1024;
    let mut storage = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
    opened_file_path_in(file, &mut storage)
}

fn opened_file_path_in(file: &File, storage: &mut [u64]) -> io::Result<PathBuf> {
    #[repr(C)]
    struct FileNameInformation {
        length: u32,
        name: [u16; 1],
    }

    let buffer_bytes = std::mem::size_of_val(storage);
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileNameInfo,
            storage.as_mut_ptr().cast(),
            buffer_bytes as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { &*storage.as_ptr().cast::<FileNameInformation>() };
    let name_bytes = information.length as usize;
    let fixed = std::mem::offset_of!(FileNameInformation, name);
    if !name_bytes.is_multiple_of(size_of::<u16>())
        || fixed
            .checked_add(name_bytes)
            .is_none_or(|extent| extent > buffer_bytes)
    {
        return Err(malformed_directory());
    }
    let wide = unsafe {
        std::slice::from_raw_parts(information.name.as_ptr(), name_bytes / size_of::<u16>())
    };
    Ok(PathBuf::from(OsString::from_wide(wide)))
}

fn opened_file_leaf_name(file: &File) -> io::Result<OsString> {
    opened_file_path(file)?
        .file_name()
        .map(OsStr::to_os_string)
        .ok_or_else(|| binding_changed("opened filesystem object has no exact leaf name"))
}

fn create_root_chain_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> Result<DirectoryHandle, CreateDirectoryError> {
    let handle = DirectoryHandle::new(
        nt_open_relative_with_attributes(
            parent,
            name,
            FILE_LIST_DIRECTORY
                | FILE_TRAVERSE_ACCESS
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_ATTRIBUTES
                | DELETE_ACCESS
                | SYNCHRONIZE_ACCESS,
            ntapi::ntioapi::FILE_CREATE,
            ntapi::ntioapi::FILE_DIRECTORY_FILE
                | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
                | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            OBJ_CASE_INSENSITIVE,
        )
        .map_err(CreateDirectoryError::NoEffect)?,
    );
    if let Err(error) = require_directory(&handle) {
        return Err(CreateDirectoryError::AppliedUnverified {
            error,
            retained: handle,
        });
    }
    Ok(handle)
}

pub(crate) fn create_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> Result<DirectoryHandle, CreateDirectoryError> {
    let handle = DirectoryHandle::new(
        nt_open_relative(
            parent,
            name,
            FILE_LIST_DIRECTORY
                | FILE_TRAVERSE_ACCESS
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_ATTRIBUTES
                | DELETE_ACCESS
                | SYNCHRONIZE_ACCESS,
            ntapi::ntioapi::FILE_CREATE,
            ntapi::ntioapi::FILE_DIRECTORY_FILE
                | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
                | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )
        .map_err(CreateDirectoryError::NoEffect)?,
    );
    if let Err(error) = require_directory(&handle) {
        return Err(CreateDirectoryError::AppliedUnverified {
            error,
            retained: handle,
        });
    }
    Ok(handle)
}

pub(crate) fn open_file(parent: &DirectoryHandle, name: &OsStr) -> io::Result<File> {
    let handle = nt_open_relative(
        parent,
        name,
        FILE_READ_DATA_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    require_file(&handle)?;
    Ok(handle)
}

pub(crate) fn open_recoverable_stage(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<File> {
    let _observation = open_file_cleanup_observation(parent, name, expected)?;
    nt_open_relative(
        parent,
        name,
        FILE_READ_DATA_ACCESS
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ,
    )
}

pub(crate) fn create_file(parent: &DirectoryHandle, name: &OsStr) -> Result<File, CreateFileError> {
    let handle = nt_open_relative(
        parent,
        name,
        FILE_READ_DATA_ACCESS
            | FILE_WRITE_DATA_ACCESS
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_CREATE,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ,
    )
    .map_err(CreateFileError::NoEffect)?;
    if let Err(error) = require_file(&handle) {
        return Err(CreateFileError::AppliedUnverified {
            error,
            retained: handle,
        });
    }
    Ok(handle)
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
    let observation = open_file_cleanup_observation(parent, name, expected)?;
    let deletion = stage.try_clone()?;
    if file_identity(&deletion)? != expected {
        return Err(binding_changed("stage deleter changed before registration"));
    }
    Ok(FileCleanupHandle {
        observation,
        deletion: Some(deletion),
    })
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
    let observation = open_file_cleanup_observation(parent, name, expected)?;
    let deletion = std::mem::replace(stage, observation);
    if let Err(error) = set_delete(&deletion) {
        let observation = std::mem::replace(stage, deletion);
        drop(observation);
        return Err(error);
    }
    drop(deletion);
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
    rename_handle_no_replace(file, parent, parent, destination_name)
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
    require_file(file)?;
    object_identity(file)
}

pub(crate) fn file_receipt_fields(file: &File) -> io::Result<(u64, FileStamp)> {
    let (basic, standard) = require_file(file)?;
    let size = u64::try_from(standard.EndOfFile)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file size is negative"))?;
    Ok((
        size,
        FileStamp {
            modified: basic.LastWriteTime,
            changed: basic.ChangeTime,
        },
    ))
}

pub(crate) fn file_content_stamp_matches(first: FileStamp, second: FileStamp) -> bool {
    first.modified == second.modified
}

pub(crate) fn file_modified_at_ns(stamp: FileStamp) -> io::Result<u64> {
    let modified = u64::try_from(stamp.modified).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "mtime precedes the Windows epoch",
        )
    })?;
    let modified_at_ns = modified
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime precedes the Unix epoch"))?
        .checked_mul(100)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime overflowed"))?;
    Ok(modified_at_ns)
}

pub(crate) fn file_changed_at_ns(stamp: FileStamp) -> io::Result<u64> {
    let changed = u64::try_from(stamp.changed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ctime precedes the Windows epoch",
        )
    })?;
    let changed_at_ns = changed
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ctime precedes the Unix epoch"))?
        .checked_mul(100)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ctime overflowed"))?;
    Ok(changed_at_ns)
}

pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(bytes, offset)
}

pub(crate) fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    file.seek_write(bytes, offset)
}

fn entry_observation(
    parent: &DirectoryHandle,
    name: &OsStr,
) -> io::Result<Option<(EntryKind, Identity)>> {
    let handle = match nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_OPEN_REPARSE_POINT | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    ) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let basic = query_basic(&handle)?;
    let standard = query_standard(&handle)?;
    if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Ok(Some((EntryKind::Link, object_identity(&handle)?)))
    } else if standard.Directory || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Ok(Some((EntryKind::Directory, object_identity(&handle)?)))
    } else {
        Ok(Some((EntryKind::File, object_identity(&handle)?)))
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
    let _enumeration = parent
        .enumeration
        .lock()
        .map_err(|_| io::Error::other("directory enumeration lock was poisoned"))?;
    let expected = directory_identity(parent)?;
    const BUFFER_BYTES: usize = 64 * 1024;
    let mut storage = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
    let mut restart = true;
    let mut observed = 0_usize;
    let mut completion = VisitCompletion::Complete;
    'pages: loop {
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        restart = false;
        let success = unsafe {
            GetFileInformationByHandleEx(
                parent.as_raw_handle(),
                class,
                storage.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if success == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                break;
            }
            return Err(error);
        }
        let mut offset = 0_usize;
        loop {
            let fixed = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let record_size = size_of::<FILE_ID_BOTH_DIR_INFO>();
            if !offset.is_multiple_of(std::mem::align_of::<FILE_ID_BOTH_DIR_INFO>())
                || offset
                    .checked_add(record_size)
                    .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(malformed_directory());
            }
            let information = unsafe {
                &*storage
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            let name_bytes = information.FileNameLength as usize;
            let extent = fixed.checked_add(name_bytes);
            if !name_bytes.is_multiple_of(size_of::<u16>())
                || extent
                    .and_then(|value| offset.checked_add(value))
                    .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(malformed_directory());
            }
            let wide = unsafe {
                std::slice::from_raw_parts(
                    information.FileName.as_ptr(),
                    name_bytes / size_of::<u16>(),
                )
            };
            let dot = u16::from(b'.');
            let is_dot = (wide.len() == 1 && wide[0] == dot)
                || (wide.len() == 2 && wide[0] == dot && wide[1] == dot);
            if !is_dot {
                if observed == limit {
                    completion = VisitCompletion::LimitExceeded;
                    break 'pages;
                }
                let kind = if information.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    EntryKind::Link
                } else if information.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                let name = OsString::from_wide(wide);
                if visitor(&name, kind)?.is_break() {
                    completion = VisitCompletion::Stopped;
                    break 'pages;
                }
                observed += 1;
            }
            if information.NextEntryOffset == 0 {
                break;
            }
            let next = information.NextEntryOffset as usize;
            if !next.is_multiple_of(size_of::<u64>()) || extent.is_none_or(|value| next < value) {
                return Err(malformed_directory());
            }
            offset = offset
                .checked_add(next)
                .filter(|value| *value < BUFFER_BYTES)
                .ok_or_else(malformed_directory)?;
        }
    }
    if directory_identity(parent)? != expected {
        return Err(binding_changed(
            "directory changed during serialized enumeration",
        ));
    }
    Ok(completion)
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
    let handle = match nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_OPEN_REPARSE_POINT | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    ) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let basic = query_basic(&handle)?;
    let standard = query_standard(&handle)?;
    Ok(is_single_link_file(&basic, &standard) && object_identity(&handle)? == expected)
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

pub(crate) fn sync_publication_file(file: &File) -> io::Result<()> {
    file.sync_all()
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
    require_ntfs_publication_volume(source, destination_parent)?;
    set_publication_write_through(source)?;
    rename_handle_no_replace(source, source_parent, destination_parent, destination_name)?;
    attempt.mark_reported();
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
    require_ntfs_publication_volume(source, parent)?;
    set_publication_write_through(source)?;
    rename_handle_no_replace(source, parent, parent, destination_name)?;
    attempt.mark_reported();
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
    )
}

/// NTFS write-through renames carry their own barrier.
pub(crate) fn settle_publication_deferring_parent_sync(
    receipt: &mut PublicationReceipt,
    attempt_id: u64,
    staged_file: &File,
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    settle_publication(
        receipt,
        attempt_id,
        staged_file,
        source_parent,
        source_name,
        destination_parent,
        destination_name,
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
    settle_publication(
        receipt,
        attempt_id,
        staged_file,
        parent,
        source_name,
        parent,
        destination_name,
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
) -> io::Result<()> {
    if !matches!(
        receipt.state,
        PublicationReceiptState::Reported | PublicationReceiptState::Committed
    ) {
        return Err(io::Error::other(
            "Windows publication has no reported NTFS write-through receipt",
        ));
    }
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
    receipt.mark_committed();
    Ok(())
}

pub(crate) fn move_file_no_replace(
    source_parent: &DirectoryHandle,
    source_name: &OsStr,
    source: &File,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    let expected = file_identity(source)?;
    if file_binding_state(source_parent, source_name, expected)? != BindingState::Exact {
        return Err(binding_changed("file binding changed before move"));
    }
    let deleter = open_file_move_deleter(source_parent, source_name, expected)?;
    rename_handle_no_replace(
        &deleter,
        source_parent,
        destination_parent,
        destination_name,
    )
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
    let deleter = open_directory_move_deleter(source_parent, source_name, expected)?;
    rename_handle_no_replace(
        &deleter,
        source_parent,
        destination_parent,
        destination_name,
    )
}

fn open_file_move_deleter(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<File> {
    let deleter = nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | DELETE_ACCESS | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    require_file(&deleter)?;
    if file_identity(&deleter)? != expected
        || file_binding_state(parent, name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "file changed while acquiring move authority",
        ));
    }
    Ok(deleter)
}

fn open_directory_move_deleter(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<File> {
    let deleter = nt_open_relative(
        parent,
        name,
        FILE_READ_ATTRIBUTES | DELETE_ACCESS | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    require_directory(&deleter)?;
    if object_identity(&deleter)? != expected
        || directory_binding_state(parent, name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "directory changed while acquiring move authority",
        ));
    }
    Ok(deleter)
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
    if let Err(error) = rename_handle_no_replace(
        cleanup
            .deletion
            .as_ref()
            .expect("new cleanup authority retains its deleter"),
        parent,
        parent,
        park_name,
    ) {
        if file_identity(source).ok() == Some(expected)
            && file_identity(&cleanup.observation).ok() == Some(expected)
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
    if file_identity(&cleanup.observation).ok() != Some(expected) {
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
    open_cleanup_file(parent, park_name, expected)
}

fn open_cleanup_file(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<FileCleanupHandle> {
    let observation = open_file_cleanup_observation(parent, name, expected)?;
    open_file_cleanup_deleter(parent, name, expected, observation).map(|(cleanup, _)| cleanup)
}

fn open_file_cleanup_observation(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<File> {
    let observation = nt_open_relative(
        parent,
        name,
        FILE_READ_DATA_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    require_file(&observation)?;
    if file_identity(&observation)? != expected {
        return Err(binding_changed(
            "cleanup observation changed before admission",
        ));
    }
    Ok(observation)
}

fn open_file_cleanup_deleter(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
    observation: File,
) -> io::Result<(FileCleanupHandle, Identity)> {
    let deletion = nt_open_relative(
        parent,
        name,
        FILE_READ_DATA_ACCESS
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ,
    )?;
    require_file(&deletion)?;
    let admitted = file_identity(&deletion)?;
    if admitted != expected || file_identity(&observation)? != expected {
        return Err(binding_changed("cleanup file changed before admission"));
    }
    Ok((
        FileCleanupHandle {
            observation,
            deletion: Some(deletion),
        },
        admitted,
    ))
}

pub(crate) fn parked_file_receipt_fields(
    parked: &FileCleanupHandle,
) -> io::Result<(u64, FileStamp)> {
    file_receipt_fields(&parked.observation)
}

pub(crate) fn parked_file_identity(parked: &FileCleanupHandle) -> io::Result<Identity> {
    file_identity(&parked.observation)
}

pub(crate) fn read_parked_at(
    parked: &FileCleanupHandle,
    bytes: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    read_at(&parked.observation, bytes, offset)
}

pub(crate) fn remove_parked_file(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut FileCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    if file_identity(&parked.observation)? != expected
        || file_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("parked file changed before removal"));
    }
    if parked.deletion.is_none() {
        let observation = parked.observation.try_clone()?;
        *parked = open_file_cleanup_deleter(parent, park_name, expected, observation)?.0;
    }
    let deletion = parked
        .deletion
        .take()
        .expect("cleanup authority admitted a deleter");
    if let Err(error) = set_delete(&deletion) {
        parked.deletion = Some(deletion);
        return Err(error);
    }
    drop(deletion);
    sync_directory(parent)?;
    let (identity, links) = retained_file_identity(&parked.observation)?;
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
    let (identity, links) = retained_file_identity(&parked.observation)?;
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
    if file_identity(&parked.observation)? != expected
        || file_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("parked file changed before restoration"));
    }
    if parked.deletion.is_none() {
        let observation = parked.observation.try_clone()?;
        *parked = open_file_cleanup_deleter(parent, park_name, expected, observation)?.0;
    }
    rename_handle_no_replace(
        parked
            .deletion
            .as_ref()
            .expect("cleanup authority admitted a deleter"),
        parent,
        parent,
        original_name,
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
    if file_identity(&parked.observation)? != expected
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
    if let Err(error) = rename_handle_no_replace(
        cleanup
            .deletion
            .as_ref()
            .expect("new cleanup authority retains its deleter"),
        parent,
        parent,
        park_name,
    ) {
        if directory_identity(source).ok() == Some(expected)
            && directory_identity(&cleanup.observation).ok() == Some(expected)
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
    if directory_identity(&cleanup.observation).ok() != Some(expected) {
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
    open_cleanup_directory(parent, park_name, expected)
}

fn open_cleanup_directory(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<DirectoryCleanupHandle> {
    let observation = open_directory_cleanup_observation(parent, name, expected)?;
    open_directory_cleanup_deleter(parent, name, expected, observation)
}

fn open_directory_cleanup_observation(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
) -> io::Result<DirectoryHandle> {
    let observation = DirectoryHandle::new(nt_open_relative(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE_ACCESS | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?);
    require_directory(&observation)?;
    if directory_identity(&observation)? != expected {
        return Err(binding_changed(
            "cleanup directory observation changed before admission",
        ));
    }
    Ok(observation)
}

fn open_directory_cleanup_deleter(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: Identity,
    observation: DirectoryHandle,
) -> io::Result<DirectoryCleanupHandle> {
    let deletion = nt_open_relative(
        parent,
        name,
        FILE_LIST_DIRECTORY
            | FILE_TRAVERSE_ACCESS
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN,
        ntapi::ntioapi::FILE_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SHARE_READ,
    )?;
    require_directory(&deletion)?;
    if object_identity(&deletion)? != expected || directory_identity(&observation)? != expected {
        return Err(binding_changed(
            "cleanup directory changed before admission",
        ));
    }
    Ok(DirectoryCleanupHandle {
        observation,
        deletion: Some(deletion),
    })
}

pub(crate) fn remove_parked_directory(
    parent: &DirectoryHandle,
    park_name: &OsStr,
    parked: &mut DirectoryCleanupHandle,
    expected: Identity,
) -> io::Result<()> {
    if directory_identity(&parked.observation)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
        || !entries(&parked.observation, 1)?.entries.is_empty()
    {
        return Err(binding_changed(
            "parked directory changed or was not empty before removal",
        ));
    }
    if parked.deletion.is_none() {
        let observation = open_directory_cleanup_observation(parent, park_name, expected)?;
        *parked = open_directory_cleanup_deleter(parent, park_name, expected, observation)?;
    }
    let deletion = parked
        .deletion
        .take()
        .expect("cleanup authority admitted a deleter");
    if let Err(error) = set_delete(&deletion) {
        parked.deletion = Some(deletion);
        return Err(error);
    }
    drop(deletion);
    sync_directory(parent)?;
    if directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || !retained_directory_is_removed(&parked.observation, expected)?
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
    if directory_identity(&parked.observation)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "parked directory tree changed before removal",
        ));
    }
    clear_directory_children(&parked.observation, &[])?;
    if directory_identity(&parked.observation)? != expected
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
        || !retained_directory_is_removed(&parked.observation, expected)?
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
    if directory_identity(&parked.observation)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed(
            "parked directory changed before restoration",
        ));
    }
    if parked.deletion.is_none() {
        let observation = open_directory_cleanup_observation(parent, park_name, expected)?;
        *parked = open_directory_cleanup_deleter(parent, park_name, expected, observation)?;
    }
    rename_handle_no_replace(
        parked
            .deletion
            .as_ref()
            .expect("cleanup authority admitted a deleter"),
        parent,
        parent,
        original_name,
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
    if directory_identity(&parked.observation)? != expected
        || directory_binding_state(parent, park_name, expected)? != BindingState::Absent
        || directory_binding_state(parent, original_name, expected)? != BindingState::Exact
    {
        return Err(binding_changed("directory restoration remains unsettled"));
    }
    open_directory(parent, original_name).map(|(handle, _)| handle)
}

fn require_ntfs_publication_volume(
    source: &File,
    destination_parent: &DirectoryHandle,
) -> io::Result<()> {
    require_local_publication_handle(source)?;
    require_local_publication_handle(&destination_parent.file)?;
    if file_identity(source)?.volume != directory_identity(destination_parent)?.volume {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named publication cannot cross filesystem volumes",
        ));
    }
    let source_serial = require_local_ntfs(source)?;
    let destination_serial = require_local_ntfs(&destination_parent.file)?;
    if source_serial != destination_serial {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named publication volume identity changed during preflight",
        ));
    }
    Ok(())
}

fn require_local_publication_handle(handle: &impl AsRawHandle) -> io::Result<()> {
    use ntapi::ntioapi::{
        FILE_IS_REMOTE_DEVICE_INFORMATION, FileIsRemoteDeviceInformation, IO_STATUS_BLOCK,
        NtQueryInformationFile,
    };

    let mut status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let mut information = unsafe { std::mem::zeroed::<FILE_IS_REMOTE_DEVICE_INFORMATION>() };
    let queried = unsafe {
        NtQueryInformationFile(
            handle.as_raw_handle().cast(),
            &mut status,
            std::ptr::addr_of_mut!(information).cast(),
            size_of::<FILE_IS_REMOTE_DEVICE_INFORMATION>() as u32,
            FileIsRemoteDeviceInformation,
        )
    };
    nt_status_result(queried)?;
    if information.IsRemote != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named durable publication requires local filesystem handles",
        ));
    }
    Ok(())
}

fn require_local_ntfs(handle: &impl AsRawHandle) -> io::Result<u32> {
    let mut serial = 0_u32;
    let mut filesystem = [0_u16; 16];
    let result = unsafe {
        GetVolumeInformationByHandleW(
            handle.as_raw_handle().cast(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "filesystem name is not terminated",
            )
        })?;
    let filesystem = String::from_utf16(&filesystem[..length]).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem name is not valid UTF-16",
        )
    })?;
    if !filesystem.eq_ignore_ascii_case("NTFS") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named durable publication requires a local NTFS volume",
        ));
    }
    Ok(serial)
}

fn set_publication_write_through(file: &File) -> io::Result<()> {
    use ntapi::ntioapi::{
        FILE_MODE_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH,
        FileModeInformation, IO_STATUS_BLOCK, NtQueryInformationFile, NtSetInformationFile,
    };

    fn query_mode(file: &File) -> io::Result<FILE_MODE_INFORMATION> {
        let mut status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
        let mut mode = unsafe { std::mem::zeroed::<FILE_MODE_INFORMATION>() };
        let queried = unsafe {
            NtQueryInformationFile(
                file.as_raw_handle().cast(),
                &mut status,
                std::ptr::addr_of_mut!(mode).cast(),
                size_of::<FILE_MODE_INFORMATION>() as u32,
                FileModeInformation,
            )
        };
        nt_status_result(queried)?;
        Ok(mode)
    }

    let mut mode = query_mode(file)?;
    if mode.Mode & FILE_SYNCHRONOUS_IO_NONALERT == 0 {
        return Err(io::Error::other(
            "staged file lost synchronous publication mode",
        ));
    }
    if mode.Mode & FILE_WRITE_THROUGH != 0 {
        return Ok(());
    }
    mode.Mode |= FILE_WRITE_THROUGH;
    let mut status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let updated = unsafe {
        NtSetInformationFile(
            file.as_raw_handle().cast(),
            &mut status,
            std::ptr::addr_of_mut!(mode).cast(),
            size_of::<FILE_MODE_INFORMATION>() as u32,
            FileModeInformation,
        )
    };
    nt_status_result(updated)?;
    let confirmed = query_mode(file)?;
    if confirmed.Mode & FILE_SYNCHRONOUS_IO_NONALERT == 0
        || confirmed.Mode & FILE_WRITE_THROUGH == 0
    {
        return Err(io::Error::other(
            "staged file did not retain synchronous write-through publication mode",
        ));
    }
    Ok(())
}

fn nt_status_result(status: i32) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    let win32 = unsafe { ntapi::ntrtl::RtlNtStatusToDosError(status) };
    Err(io::Error::from_raw_os_error(win32 as i32))
}

fn rename_handle_no_replace(
    source: &File,
    source_parent: &DirectoryHandle,
    destination_parent: &DirectoryHandle,
    destination_name: &OsStr,
) -> io::Result<()> {
    use ntapi::ntioapi::{
        FILE_RENAME_INFORMATION, FileRenameInformation, IO_STATUS_BLOCK, NtSetInformationFile,
    };
    use ntapi::ntrtl::RtlNtStatusToDosError;

    let encoded = encode_leaf(destination_name)?;
    let filename_bytes = encoded
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(name_too_long)?;
    // FILE_RENAME_INFORMATION includes one UTF-16 slot; allocation follows the
    // documented structure size plus the exact variable filename bytes.
    let buffer_bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(filename_bytes as usize)
        .ok_or_else(name_too_long)?;
    let mut storage = vec![0_usize; buffer_bytes.div_ceil(size_of::<usize>())];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let same_parent = directory_identity(source_parent)? == directory_identity(destination_parent)?;
    unsafe {
        (*information).ReplaceIfExists = 0;
        (*information).RootDirectory = if same_parent {
            std::ptr::null_mut()
        } else {
            destination_parent.as_raw_handle().cast()
        };
        (*information).FileNameLength = filename_bytes;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            encoded.len(),
        );
    }
    let mut status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let renamed = unsafe {
        NtSetInformationFile(
            source.as_raw_handle().cast(),
            &mut status,
            information.cast(),
            buffer_bytes as u32,
            FileRenameInformation,
        )
    };
    if renamed < 0 {
        let win32 = unsafe { RtlNtStatusToDosError(renamed) };
        Err(io::Error::from_raw_os_error(win32 as i32))
    } else {
        Ok(())
    }
}

pub(crate) fn sync_directory(_directory: &DirectoryHandle) -> io::Result<()> {
    Ok(())
}

pub(crate) fn try_acquire_lease(root: &RootGuard, name: &OsStr) -> LeaseAcquisitionOutcome {
    if let Err(error) = validate_root(root) {
        return LeaseAcquisitionOutcome::NoEffect(error);
    }
    let name_class_revision = match validate_windows_lease_name_class(root, name, false) {
        Ok(revision) => RwLock::new(revision),
        Err(error) => return LeaseAcquisitionOutcome::NoEffect(error),
    };
    let opened = match nt_open_relative_with_information(
        &root.handle,
        name,
        FILE_READ_DATA_ACCESS
            | FILE_WRITE_DATA_ACCESS
            | FILE_READ_ATTRIBUTES
            | FILE_WRITE_ATTRIBUTES
            | DELETE_ACCESS
            | SYNCHRONIZE_ACCESS,
        ntapi::ntioapi::FILE_OPEN_IF,
        ntapi::ntioapi::FILE_NON_DIRECTORY_FILE
            | ntapi::ntioapi::FILE_OPEN_REPARSE_POINT
            | ntapi::ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
        0,
        0,
    ) {
        Ok(opened) => opened,
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
            return LeaseAcquisitionOutcome::NoEffect(io::Error::new(
                io::ErrorKind::WouldBlock,
                error,
            ));
        }
        Err(error) => return LeaseAcquisitionOutcome::NoEffect(error),
    };
    let state = match opened.information {
        value if value == ntapi::ntioapi::FILE_CREATED as usize => {
            LeaseAcquisitionState::Created { identity: None }
        }
        value if value == ntapi::ntioapi::FILE_OPENED as usize => {
            LeaseAcquisitionState::Opened { identity: None }
        }
        _ => LeaseAcquisitionState::Unclassified { identity: None },
    };
    if let Err(error) = require_file(&opened.handle) {
        return lease_acquisition_failure(error, opened.handle, state, name);
    }
    let identity = match object_identity(&opened.handle) {
        Ok(identity) => identity,
        Err(error) => {
            return lease_acquisition_failure(error, opened.handle, state, name);
        }
    };
    if let Err(error) = validate_root(root) {
        let mut state = state;
        state.retain_identity(identity);
        return lease_acquisition_failure(error, opened.handle, state, name);
    }
    if let Err(error) =
        validate_windows_lease_binding(root, name, &opened.handle, identity, &name_class_revision)
    {
        let mut state = state;
        state.retain_identity(identity);
        return lease_acquisition_failure(error, opened.handle, state, name);
    }
    let retained_root = match retain_root_proof(root) {
        Ok(root) => root,
        Err(error) => {
            let mut state = state;
            state.retain_identity(identity);
            return lease_acquisition_failure(error, opened.handle, state, name);
        }
    };
    LeaseAcquisitionOutcome::Acquired(LeaseHandle {
        handle: opened.handle,
        identity,
        root: retained_root,
        name: name.to_os_string(),
        name_class_revision,
    })
}

fn lease_acquisition_failure(
    error: io::Error,
    handle: File,
    state: LeaseAcquisitionState,
    name: &OsStr,
) -> LeaseAcquisitionOutcome {
    if matches!(&state, LeaseAcquisitionState::Opened { .. }) {
        LeaseAcquisitionOutcome::NoEffect(error)
    } else {
        LeaseAcquisitionOutcome::AppliedUnverified(LeaseAcquisitionObligation {
            error,
            handle: Some(handle),
            name: name.to_os_string(),
            state,
        })
    }
}

pub(crate) fn reconcile_lease_acquisition(
    root: &RootGuard,
    mut obligation: LeaseAcquisitionObligation,
) -> Result<LeaseHandle, LeaseAcquisitionObligation> {
    if let Err(error) = validate_root(root) {
        obligation.error = error;
        return Err(obligation);
    }
    let Some(handle) = obligation.handle.as_ref() else {
        obligation.error =
            io::Error::other("lease acquisition no longer retains its native handle");
        return Err(obligation);
    };
    if let Err(error) = require_file(handle) {
        obligation.error = error;
        return Err(obligation);
    }
    let identity = match object_identity(handle) {
        Ok(identity) => identity,
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    if obligation
        .state
        .identity()
        .is_some_and(|expected| expected != identity)
    {
        obligation.error = binding_changed("lease acquisition handle changed identity");
        return Err(obligation);
    }
    obligation.state.retain_identity(identity);
    let name_class_revision = RwLock::new(None);
    if let Err(error) = validate_windows_lease_binding(
        root,
        &obligation.name,
        handle,
        identity,
        &name_class_revision,
    ) {
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
            .expect("reconciled lease acquisition retains its handle"),
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
    if matches!(
        &obligation.state,
        LeaseAcquisitionState::DeletionAdmitted { .. }
    ) {
        return match sync_directory(&root.handle) {
            Ok(()) => Ok(()),
            Err(error) => {
                obligation.error = error;
                Err(obligation)
            }
        };
    }
    if !obligation.state.is_created() {
        obligation.error = io::Error::other("lease acquisition creation disposition is not exact");
        return Err(obligation);
    }
    if obligation.handle.is_none() {
        obligation.error =
            io::Error::other("created lease cleanup lost its retained native handle");
        return Err(obligation);
    }
    let handle = obligation
        .handle
        .as_ref()
        .expect("created lease cleanup retains its native handle");
    if let Err(error) = require_file(handle) {
        obligation.error = error;
        return Err(obligation);
    }
    let identity = match object_identity(handle) {
        Ok(identity) => identity,
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    if obligation
        .state
        .identity()
        .is_some_and(|expected| expected != identity)
    {
        obligation.error = binding_changed("created lease cleanup identity changed");
        return Err(obligation);
    }
    obligation.state.retain_identity(identity);
    let name_class_revision = RwLock::new(None);
    if let Err(error) =
        validate_windows_lease_binding(root, name, handle, identity, &name_class_revision)
    {
        obligation.error = error;
        return Err(obligation);
    }
    if let Err(error) = set_delete(handle) {
        obligation.error = error;
        return Err(obligation);
    }
    let standard = match query_standard(handle) {
        Ok(standard) => standard,
        Err(error) => {
            obligation.error = error;
            return Err(obligation);
        }
    };
    if !standard.DeletePending && standard.NumberOfLinks != 0 {
        obligation.error = binding_changed("created lease cleanup did not retain deletion state");
        return Err(obligation);
    }
    obligation.state = LeaseAcquisitionState::DeletionAdmitted { identity };
    drop(obligation.handle.take());
    match sync_directory(&root.handle) {
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
    validate_windows_lease_binding(
        &lease.root,
        &lease.name,
        &lease.handle,
        lease.identity,
        &lease.name_class_revision,
    )
}

fn validate_windows_lease_binding(
    root: &RootGuard,
    name: &OsStr,
    handle: &File,
    expected: Identity,
    name_class_revision: &RwLock<Option<DirectoryStamp>>,
) -> io::Result<()> {
    validate_root(root)?;
    validate_windows_lease_direct_binding(root, name, handle, expected)?;
    validate_cached_windows_lease_name_class(root, name, handle, expected, name_class_revision)
}

fn validate_windows_lease_direct_binding(
    root: &RootGuard,
    name: &OsStr,
    handle: &File,
    expected: Identity,
) -> io::Result<()> {
    require_file(handle)?;
    let mut expected_path = opened_file_path(&root.handle.file)?;
    expected_path.push(name);
    let standard = query_standard(handle)?;
    if object_identity(handle)? != expected
        || standard.NumberOfLinks != 1
        || opened_file_path(handle)? != expected_path
    {
        return Err(binding_changed("application root lease binding changed"));
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LeaseNameClassState {
    Absent,
    Exact,
    Invalid,
}

fn observe_windows_lease_name_class(
    root: &RootGuard,
    name: &OsStr,
) -> io::Result<LeaseNameClassState> {
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

fn validate_windows_lease_name_class(
    root: &RootGuard,
    name: &OsStr,
    require_exact: bool,
) -> io::Result<Option<DirectoryStamp>> {
    for attempt in 0..LEASE_NAME_CLASS_VALIDATION_ATTEMPTS {
        let revision = directory_revision(&root.handle)?;
        let state = observe_windows_lease_name_class(root, name)?;
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

fn validate_cached_windows_lease_name_class(
    root: &RootGuard,
    name: &OsStr,
    handle: &File,
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
        validate_windows_lease_direct_binding(root, name, handle, expected)?;
        let revision = directory_revision(&root.handle)?;
        if cached_revision.as_ref() == Some(&revision) {
            return Ok(());
        }
        *cached_revision = None;
        let state = observe_windows_lease_name_class(root, name)?;
        validate_windows_lease_direct_binding(root, name, handle, expected)?;
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

pub(crate) fn recovery_control_initialize_len(lease: &LeaseHandle) -> io::Result<()> {
    validate_lease(lease)?;
    let length = lease.handle.metadata()?.len();
    if length == 0 {
        lease.handle.set_len(RECOVERY_CONTROL_BYTES)?;
        lease.handle.sync_all()?;
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
    #[cfg(test)]
    run_recovery_control_read_test_hook(lease)?;
    recovery_control_read_exact_with(offset, bytes, |bytes, position| {
        lease.handle.seek_read(bytes, position)
    })?;
    validate_recovery_control(lease)
}

#[cfg(test)]
pub(crate) fn set_recovery_control_len_for_test(
    lease: &LeaseHandle,
    length: u64,
) -> io::Result<()> {
    lease.handle.set_len(length)
}

pub(crate) fn recovery_control_write_all_at(
    lease: &LeaseHandle,
    offset: u64,
    bytes: &[u8],
) -> io::Result<()> {
    validate_recovery_control_range(lease, offset, bytes.len())?;
    recovery_control_write_all_with(offset, bytes, |bytes, position| {
        lease.handle.seek_write(bytes, position)
    })?;
    validate_recovery_control(lease)
}

pub(crate) fn recovery_control_sync(lease: &LeaseHandle) -> Result<(), RecoveryControlSyncError> {
    validate_recovery_control(lease).map_err(RecoveryControlSyncError::before_barrier)?;
    lease
        .handle
        .sync_all()
        .map_err(RecoveryControlSyncError::before_barrier)?;
    validate_recovery_control(lease).map_err(RecoveryControlSyncError::after_barrier)
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

fn nt_open_relative(
    parent: &DirectoryHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    share: u32,
) -> io::Result<File> {
    nt_open_relative_with_attributes(parent, name, desired_access, disposition, options, share, 0)
}

fn nt_open_relative_with_attributes(
    parent: &DirectoryHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    share: u32,
    object_flags: u32,
) -> io::Result<File> {
    Ok(nt_open_relative_with_information(
        parent,
        name,
        desired_access,
        disposition,
        options,
        share,
        object_flags,
    )?
    .handle)
}

fn nt_open_relative_with_information(
    parent: &DirectoryHandle,
    name: &OsStr,
    desired_access: u32,
    disposition: u32,
    options: u32,
    share: u32,
    object_flags: u32,
) -> io::Result<NtOpenResult> {
    use ntapi::ntioapi::{IO_STATUS_BLOCK, NtCreateFile};
    use ntapi::ntrtl::RtlNtStatusToDosError;
    use ntapi::winapi::shared::ntdef::{
        InitializeObjectAttributes, OBJECT_ATTRIBUTES, UNICODE_STRING,
    };

    let mut encoded = encode_leaf(name)?;
    let byte_len = encoded
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(name_too_long)?;
    let mut unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: encoded.as_mut_ptr(),
    };
    let mut attributes = unsafe { std::mem::zeroed::<OBJECT_ATTRIBUTES>() };
    unsafe {
        InitializeObjectAttributes(
            &mut attributes,
            &mut unicode,
            object_flags,
            parent.as_raw_handle().cast(),
            std::ptr::null_mut(),
        );
    }
    let mut status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let mut handle = std::ptr::null_mut();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &mut attributes,
            &mut status,
            std::ptr::null_mut(),
            0,
            share,
            disposition,
            options,
            std::ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        let win32 = unsafe { RtlNtStatusToDosError(result) };
        Err(io::Error::from_raw_os_error(win32 as i32))
    } else if handle.is_null() {
        Err(io::Error::other(
            "NtCreateFile succeeded without returning a handle",
        ))
    } else {
        // NT_SUCCESS transfers one valid owned handle through the non-null
        // output slot; failure leaves that slot unspecified and is never wrapped.
        Ok(NtOpenResult {
            handle: unsafe { File::from_raw_handle(handle.cast()) },
            information: status.Information,
        })
    }
}

fn encode_leaf(name: &OsStr) -> io::Result<Vec<u16>> {
    let encoded = name.encode_wide().collect::<Vec<_>>();
    if encoded.is_empty()
        || encoded == [b'.' as u16]
        || encoded == [b'.' as u16, b'.' as u16]
        || encoded
            .iter()
            .any(|unit| matches!(*unit, 0 | 0x2f | 0x3a | 0x5c))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem capability leaf name is invalid",
        ));
    }
    Ok(encoded)
}

fn require_directory(handle: &File) -> io::Result<()> {
    let basic = query_basic(handle)?;
    let standard = query_standard(handle)?;
    if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || !standard.Directory
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem capability is not an exact directory",
        ));
    }
    Ok(())
}

fn require_file(handle: &File) -> io::Result<(FILE_BASIC_INFO, FILE_STANDARD_INFO)> {
    let basic = query_basic(handle)?;
    let standard = query_standard(handle)?;
    if !is_single_link_file(&basic, &standard) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "filesystem capability is not an exact single-link regular file",
        ));
    }
    Ok((basic, standard))
}

fn is_single_link_file(basic: &FILE_BASIC_INFO, standard: &FILE_STANDARD_INFO) -> bool {
    basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) == 0
        && !standard.Directory
        && standard.NumberOfLinks == 1
}

fn retained_file_identity(handle: &File) -> io::Result<(Identity, u32)> {
    let basic = query_basic(handle)?;
    let standard = query_standard(handle)?;
    if basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
        || standard.Directory
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "retained cleanup capability is not a regular file",
        ));
    }
    Ok((object_identity(handle)?, standard.NumberOfLinks))
}

fn object_identity(handle: &File) -> io::Result<Identity> {
    let info = query_id(handle)?;
    Ok(Identity {
        volume: info.VolumeSerialNumber,
        id: info.FileId.Identifier,
    })
}

fn set_delete(handle: &File) -> io::Result<()> {
    let mut disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let success = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileDispositionInfoEx,
            (&mut disposition as *mut FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn query_basic(handle: &File) -> io::Result<FILE_BASIC_INFO> {
    let mut value = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    let success = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileBasicInfo,
            value.as_mut_ptr().cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        // The API initializes the complete fixed structure on success.
        Ok(unsafe { value.assume_init() })
    }
}

fn query_standard(handle: &File) -> io::Result<FILE_STANDARD_INFO> {
    let mut value = MaybeUninit::<FILE_STANDARD_INFO>::uninit();
    let success = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileStandardInfo,
            value.as_mut_ptr().cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        // The API initializes the complete fixed structure on success.
        Ok(unsafe { value.assume_init() })
    }
}

fn query_id(handle: &File) -> io::Result<FILE_ID_INFO> {
    let mut value = MaybeUninit::<FILE_ID_INFO>::uninit();
    let success = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            value.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        // The API initializes the complete fixed structure on success.
        Ok(unsafe { value.assume_init() })
    }
}

fn malformed_directory() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "directory enumeration returned a malformed record",
    )
}

fn name_too_long() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "leaf name is too long")
}

fn binding_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
