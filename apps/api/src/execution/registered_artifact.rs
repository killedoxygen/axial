//! Confined mutation of one exact registered launcher-managed artifact.

use super::file::file_fact;
use super::{ExecutionFact, ExecutionFactKind};
use crate::state::contracts::{OperationId, TargetDescriptor};
use axial_minecraft::known_good::KnownGoodPhysicalPath;
use futures_util::StreamExt;
use reqwest::Client;
use sha1::{Digest as _, Sha1};
use std::io;

pub(crate) struct RegisteredArtifactMutationCapability {
    #[cfg(unix)]
    inner: unix::ConfinedLeaf,
    #[cfg(windows)]
    inner: windows::ConfinedLeaf,
}

pub(crate) struct RegisteredArtifactMutationReport {
    pub(crate) facts: Vec<ExecutionFact>,
}

pub(crate) struct RegisteredArtifactMutationError {
    pub(crate) facts: Vec<ExecutionFact>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredArtifactPhysicalState {
    Missing,
    Exact,
    Corrupt,
}

impl RegisteredArtifactMutationCapability {
    pub(crate) async fn mint(path: KnownGoodPhysicalPath) -> io::Result<Self> {
        #[cfg(unix)]
        {
            return tokio::task::spawn_blocking(move || {
                unix::ConfinedLeaf::mint(path).map(|inner| Self { inner })
            })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        }
        #[cfg(windows)]
        {
            return tokio::task::spawn_blocking(move || {
                windows::ConfinedLeaf::mint(path).map(|inner| Self { inner })
            })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "registered artifact confined mutation is unavailable on this platform",
            ))
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        #[cfg(unix)]
        {
            return self.inner.revalidate().is_ok();
        }
        #[cfg(windows)]
        {
            return self.inner.revalidate().is_ok();
        }
        #[cfg(not(any(unix, windows)))]
        false
    }

    pub(crate) fn target_is_missing(&self) -> bool {
        #[cfg(unix)]
        {
            return self.inner.target_is_missing().unwrap_or(false);
        }
        #[cfg(windows)]
        {
            return self.inner.target_is_missing().unwrap_or(false);
        }
        #[cfg(not(any(unix, windows)))]
        false
    }

    pub(crate) fn quarantine_existing(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
    ) -> Result<RegisteredArtifactMutationReport, RegisteredArtifactMutationError> {
        #[cfg(unix)]
        {
            return self
                .inner
                .quarantine_existing()
                .map(|()| RegisteredArtifactMutationReport {
                    facts: vec![file_fact(
                        ExecutionFactKind::FileQuarantined,
                        Some(operation_id.clone()),
                        target,
                    )],
                })
                .map_err(|error| mutation_error(error, operation_id, target));
        }
        #[cfg(windows)]
        {
            return self
                .inner
                .quarantine_existing()
                .map(|()| RegisteredArtifactMutationReport {
                    facts: vec![file_fact(
                        ExecutionFactKind::FileQuarantined,
                        Some(operation_id.clone()),
                        target,
                    )],
                })
                .map_err(|error| mutation_error(error, operation_id, target));
        }
        #[cfg(not(any(unix, windows)))]
        Err(mutation_error(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "confined quarantine unavailable",
            ),
            operation_id,
            target,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_verify_promote(
        &self,
        operation_id: &OperationId,
        target: &TargetDescriptor,
        provider_url: &str,
        expected_sha1: &str,
        expected_size: u64,
        client: &Client,
    ) -> Result<RegisteredArtifactMutationReport, RegisteredArtifactMutationError> {
        #[cfg(any(unix, windows))]
        {
            if !self.is_current() || !self.target_is_missing() {
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined leaf changed"),
                    operation_id,
                    target,
                ));
            }
            let mut temp = self
                .inner
                .create_temp()
                .map_err(|error| mutation_error(error, operation_id, target))?;
            let temp_file = temp.take_writer().ok_or_else(|| {
                mutation_error(
                    io::Error::other("confined temp writer is unavailable"),
                    operation_id,
                    target,
                )
            })?;
            let result = stream_exact_artifact(
                temp_file,
                provider_url,
                expected_sha1,
                expected_size,
                client,
            )
            .await;
            if let Err(kind) = result {
                self.inner.remove_temp(temp);
                return Err(RegisteredArtifactMutationError {
                    facts: vec![execution_fact(kind, operation_id, target)],
                });
            }
            if !self.is_current() || !self.target_is_missing() {
                self.inner.remove_temp(temp);
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined leaf changed"),
                    operation_id,
                    target,
                ));
            }
            if let Err(error) = self.inner.promote_temp(&temp) {
                self.inner.remove_temp(temp);
                return Err(mutation_error(error, operation_id, target));
            }
            if !self.is_current() {
                return Err(mutation_error(
                    io::Error::new(io::ErrorKind::PermissionDenied, "confined parent changed"),
                    operation_id,
                    target,
                ));
            }
            return Ok(RegisteredArtifactMutationReport {
                facts: vec![
                    execution_fact(
                        ExecutionFactKind::DownloadWrittenToTemp,
                        operation_id,
                        target,
                    ),
                    execution_fact(ExecutionFactKind::DownloadPromoted, operation_id, target),
                ],
            });
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (provider_url, expected_sha1, expected_size, client);
            Err(mutation_error(
                io::Error::new(io::ErrorKind::Unsupported, "confined download unavailable"),
                operation_id,
                target,
            ))
        }
    }

    pub(crate) async fn verify_exact(&self, expected_sha1: &str, expected_size: u64) -> bool {
        #[cfg(any(unix, windows))]
        {
            if !self.is_current() {
                return false;
            }
            let expected_sha1 = expected_sha1.to_string();
            let Some(verification) = self.inner.verification() else {
                return false;
            };
            let verified = tokio::task::spawn_blocking(move || {
                verification.verify(&expected_sha1, expected_size)
            })
            .await
            .unwrap_or(false);
            return verified && self.is_current();
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (expected_sha1, expected_size);
            false
        }
    }

    pub(crate) async fn classify(
        &self,
        expected_sha1: &str,
        expected_size: u64,
    ) -> Option<RegisteredArtifactPhysicalState> {
        if !self.is_current() {
            return None;
        }
        if self.verify_exact(expected_sha1, expected_size).await {
            return Some(RegisteredArtifactPhysicalState::Exact);
        }
        if !self.is_current() {
            return None;
        }
        if self.target_is_missing() {
            Some(RegisteredArtifactPhysicalState::Missing)
        } else {
            Some(RegisteredArtifactPhysicalState::Corrupt)
        }
    }
}

fn mutation_error(
    error: io::Error,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> RegisteredArtifactMutationError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => ExecutionFactKind::FileMissing,
        io::ErrorKind::PermissionDenied => ExecutionFactKind::FilePermissionDenied,
        _ => ExecutionFactKind::PrimitiveRefused,
    };
    RegisteredArtifactMutationError {
        facts: vec![file_fact(kind, Some(operation_id.clone()), target)],
    }
}

fn execution_fact(
    kind: ExecutionFactKind,
    operation_id: &OperationId,
    target: &TargetDescriptor,
) -> ExecutionFact {
    file_fact(kind, Some(operation_id.clone()), target)
}

#[cfg(any(unix, windows))]
async fn stream_exact_artifact(
    file: std::fs::File,
    provider_url: &str,
    expected_sha1: &str,
    expected_size: u64,
    client: &Client,
) -> Result<(), ExecutionFactKind> {
    use tokio::io::AsyncWriteExt as _;

    let response = client
        .get(provider_url)
        .send()
        .await
        .map_err(|_| ExecutionFactKind::DownloadNetworkFailure)?;
    if !response.status().is_success() {
        return Err(ExecutionFactKind::DownloadProviderFailure);
    }
    if response
        .content_length()
        .is_some_and(|length| length != expected_size)
    {
        return Err(ExecutionFactKind::DownloadSizeMismatch);
    }
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ExecutionFactKind::DownloadInterrupted)?;
        observed = observed
            .checked_add(chunk.len() as u64)
            .ok_or(ExecutionFactKind::DownloadSizeMismatch)?;
        if observed > expected_size {
            return Err(ExecutionFactKind::DownloadSizeMismatch);
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)?;
        hasher.update(&chunk);
    }
    if observed != expected_size {
        return Err(ExecutionFactKind::DownloadSizeMismatch);
    }
    if format!("{:x}", hasher.finalize()) != expected_sha1 {
        return Err(ExecutionFactKind::DownloadChecksumMismatch);
    }
    file.flush()
        .await
        .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)?;
    file.sync_all()
        .await
        .map_err(|_| ExecutionFactKind::DownloadTempWriteFailed)
}

#[cfg(unix)]
mod unix {
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
    use sha1::{Digest as _, Sha1};
    use std::ffi::OsString;
    use std::io::{self, Read as _};
    use std::path::{Component, PathBuf};
    use std::sync::Arc;

    #[derive(Clone)]
    struct HeldDirectory {
        handle: Arc<OwnedFd>,
        parent: Option<Arc<OwnedFd>>,
        name: Option<OsString>,
        device: u64,
        inode: u64,
    }

    pub(super) struct ConfinedLeaf {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<OwnedFd>,
        leaf: OsString,
    }

    pub(super) struct Verification {
        parent: Arc<OwnedFd>,
        leaf: OsString,
        file: std::fs::File,
        device: u64,
        inode: u64,
        modified_seconds: i64,
        modified_nanoseconds: u64,
        changed_seconds: i64,
        changed_nanoseconds: u64,
    }

    pub(super) struct Temp {
        name: OsString,
        writer: Option<std::fs::File>,
        control: std::fs::File,
        device: u64,
        inode: u64,
    }

    impl Temp {
        pub(super) fn take_writer(&mut self) -> Option<std::fs::File> {
            self.writer.take()
        }
    }

    impl ConfinedLeaf {
        pub(super) fn mint(path: KnownGoodPhysicalPath) -> io::Result<Self> {
            let root_path = PathBuf::from("/");
            let mut directories = open_absolute_directory_chain(path.root())?;
            let mut parent = directories
                .last()
                .expect("absolute directory chain has anchor")
                .handle
                .clone();
            let mut components = path.relative().components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact path escaped its root",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                let child = match rustix::fs::openat(
                    parent.as_ref(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(child) => child,
                    Err(error) => return Err(io::Error::from(error)),
                };
                let stat = rustix::fs::fstat(&child).map_err(io::Error::from)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact ancestor is not a directory",
                    ));
                }
                let child = Arc::new(child);
                directories.push(HeldDirectory {
                    handle: child.clone(),
                    parent: Some(parent),
                    name: Some(name.to_os_string()),
                    device: stat.st_dev,
                    inode: stat.st_ino,
                });
                parent = child;
            }
            let leaf = leaf.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact leaf is empty",
                )
            })?;
            let capability = Self {
                root_path,
                directories,
                parent,
                leaf,
            };
            capability.revalidate()?;
            Ok(capability)
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            let root = rustix::fs::open(
                &self.root_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let root_stat = rustix::fs::fstat(&root).map_err(io::Error::from)?;
            let expected_root = &self.directories[0];
            if root_stat.st_dev != expected_root.device || root_stat.st_ino != expected_root.inode {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact root changed",
                ));
            }
            let held_root =
                rustix::fs::fstat(expected_root.handle.as_ref()).map_err(io::Error::from)?;
            if held_root.st_dev != expected_root.device || held_root.st_ino != expected_root.inode {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact held root changed",
                ));
            }
            for directory in self.directories.iter().skip(1) {
                let stat = rustix::fs::statat(
                    directory
                        .parent
                        .as_ref()
                        .expect("child has held parent")
                        .as_ref(),
                    directory.name.as_ref().expect("child has name"),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io::Error::from)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
                    || stat.st_dev != directory.device
                    || stat.st_ino != directory.inode
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact ancestor changed",
                    ));
                }
                let held = rustix::fs::fstat(directory.handle.as_ref()).map_err(io::Error::from)?;
                if held.st_dev != directory.device || held.st_ino != directory.inode {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact held ancestor changed",
                    ));
                }
            }
            Ok(())
        }

        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            match rustix::fs::statat(self.parent.as_ref(), &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => Ok(false),
                Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(io::Error::from(error)),
            }
        }

        pub(super) fn quarantine_existing(&self) -> io::Result<()> {
            self.revalidate()?;
            let file = rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let source = rustix::fs::fstat(&file).map_err(io::Error::from)?;
            if FileType::from_raw_mode(source.st_mode) != FileType::RegularFile {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact leaf is not a regular file",
                ));
            }
            let quarantine = OsString::from(format!(
                ".axial-quarantine-{}",
                uuid::Uuid::new_v4().simple()
            ));
            rustix::fs::renameat_with(
                self.parent.as_ref(),
                &self.leaf,
                self.parent.as_ref(),
                &quarantine,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
            rustix::fs::fsync(self.parent.as_ref()).map_err(io::Error::from)?;
            self.revalidate()?;
            if !self.target_is_missing()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact quarantine did not vacate leaf",
                ));
            }
            let quarantined =
                rustix::fs::statat(self.parent.as_ref(), &quarantine, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            if quarantined.st_dev != source.st_dev || quarantined.st_ino != source.st_ino {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact quarantine identity changed",
                ));
            }
            Ok(())
        }

        pub(super) fn create_temp(&self) -> io::Result<Temp> {
            self.revalidate()?;
            let name = OsString::from(format!(
                ".axial-repair-{}.tmp",
                uuid::Uuid::new_v4().simple()
            ));
            let handle = rustix::fs::openat(
                self.parent.as_ref(),
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(io::Error::from)?;
            let writer = std::fs::File::from(handle);
            let control = writer.try_clone()?;
            let stat = rustix::fs::fstat(&control).map_err(io::Error::from)?;
            Ok(Temp {
                name,
                writer: Some(writer),
                control,
                device: stat.st_dev,
                inode: stat.st_ino,
            })
        }

        pub(super) fn remove_temp(&self, temp: Temp) {
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW);
            if current
                .is_ok_and(|current| current.st_dev == temp.device && current.st_ino == temp.inode)
            {
                let _ = rustix::fs::unlinkat(self.parent.as_ref(), &temp.name, AtFlags::empty());
            }
            let _ = rustix::fs::fsync(self.parent.as_ref());
        }

        pub(super) fn promote_temp(&self, temp: &Temp) -> io::Result<()> {
            let held = rustix::fs::fstat(&temp.control).map_err(io::Error::from)?;
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            if held.st_dev != temp.device
                || held.st_ino != temp.inode
                || current.st_dev != temp.device
                || current.st_ino != temp.inode
                || FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact temp changed",
                ));
            }
            rustix::fs::renameat_with(
                self.parent.as_ref(),
                &temp.name,
                self.parent.as_ref(),
                &self.leaf,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
            rustix::fs::fsync(self.parent.as_ref()).map_err(io::Error::from)
        }

        pub(super) fn verification(&self) -> Option<Verification> {
            self.revalidate().ok()?;
            let handle = rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()?;
            let stat = rustix::fs::fstat(&handle).ok()?;
            (FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile).then(|| Verification {
                parent: self.parent.clone(),
                leaf: self.leaf.clone(),
                file: std::fs::File::from(handle),
                device: stat.st_dev,
                inode: stat.st_ino,
                modified_seconds: stat.st_mtime,
                modified_nanoseconds: stat.st_mtime_nsec,
                changed_seconds: stat.st_ctime,
                changed_nanoseconds: stat.st_ctime_nsec,
            })
        }
    }

    impl Verification {
        pub(super) fn verify(mut self, expected_sha1: &str, expected_size: u64) -> bool {
            let before = match self.file.metadata() {
                Ok(metadata) if metadata.is_file() && metadata.len() == expected_size => metadata,
                _ => return false,
            };
            let mut hasher = Sha1::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = match self.file.read(&mut buffer) {
                    Ok(count) => count,
                    Err(_) => return false,
                };
                if count == 0 {
                    break;
                }
                observed = match observed.checked_add(count as u64) {
                    Some(observed) if observed <= expected_size => observed,
                    _ => return false,
                };
                hasher.update(&buffer[..count]);
            }
            let held = match rustix::fs::fstat(&self.file) {
                Ok(held) => held,
                Err(_) => return false,
            };
            if observed != expected_size
                || before.len() != expected_size
                || held.st_dev != self.device
                || held.st_ino != self.inode
                || held.st_mtime != self.modified_seconds
                || held.st_mtime_nsec != self.modified_nanoseconds
                || held.st_ctime != self.changed_seconds
                || held.st_ctime_nsec != self.changed_nanoseconds
                || format!("{:x}", hasher.finalize()) != expected_sha1
            {
                return false;
            }
            let current = match rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(current) => current,
                Err(_) => return false,
            };
            rustix::fs::fstat(&current).is_ok_and(|current| {
                FileType::from_raw_mode(current.st_mode) == FileType::RegularFile
                    && current.st_dev == self.device
                    && current.st_ino == self.inode
            })
        }
    }

    fn open_absolute_directory_chain(path: &std::path::Path) -> io::Result<Vec<HeldDirectory>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered artifact root is not absolute",
            ));
        }
        let root = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let stat = rustix::fs::fstat(&root).map_err(io::Error::from)?;
        let root = Arc::new(root);
        let mut directories = vec![HeldDirectory {
            handle: root.clone(),
            parent: None,
            name: None,
            device: stat.st_dev,
            inode: stat.st_ino,
        }];
        let mut parent = root;
        for component in path.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact root is not a normalized absolute path",
                    ));
                }
            };
            let child = rustix::fs::openat(
                parent.as_ref(),
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let stat = rustix::fs::fstat(&child).map_err(io::Error::from)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact root ancestor is not a directory",
                ));
            }
            let child = Arc::new(child);
            directories.push(HeldDirectory {
                handle: child.clone(),
                parent: Some(parent),
                name: Some(name.to_os_string()),
                device: stat.st_dev,
                inode: stat.st_ino,
            });
            parent = child;
        }
        Ok(directories)
    }
}

#[cfg(windows)]
mod windows {
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use sha1::{Digest as _, Sha1};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, Read as _};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Component, Path, PathBuf, Prefix};
    use std::ptr;
    use std::sync::Arc;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
        FileRenameInformation, NtCreateFile, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE,
        RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FILE_TRAVERSE, FileBasicInfo, FileDispositionInfo, FileIdInfo,
        FileStandardInfo, GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    #[derive(Clone)]
    struct HeldDirectory {
        handle: Arc<fs::File>,
        parent: Option<Arc<fs::File>>,
        name: Option<OsString>,
        volume: u64,
        id: [u8; 16],
    }

    pub(super) struct ConfinedLeaf {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<fs::File>,
        leaf: OsString,
    }

    pub(super) struct Verification {
        parent: Arc<fs::File>,
        leaf: OsString,
        file: fs::File,
        volume: u64,
        id: [u8; 16],
        modified: i64,
        changed: i64,
    }

    pub(super) struct Temp {
        writer: Option<fs::File>,
        control: fs::File,
        volume: u64,
        id: [u8; 16],
    }

    impl Temp {
        pub(super) fn take_writer(&mut self) -> Option<fs::File> {
            self.writer.take()
        }
    }

    impl ConfinedLeaf {
        pub(super) fn mint(path: KnownGoodPhysicalPath) -> io::Result<Self> {
            let (root_path, mut directories) = open_absolute_directory_chain(path.root())?;
            let mut parent = directories
                .last()
                .expect("absolute directory chain has anchor")
                .handle
                .clone();
            let mut components = path.relative().components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact path escaped its root",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                let child = Arc::new(open_relative(
                    &parent,
                    name,
                    Some(true),
                    FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                )?);
                require_exact_directory(&child)?;
                let id = query::<FILE_ID_INFO>(&child, FileIdInfo)?;
                directories.push(HeldDirectory {
                    handle: child.clone(),
                    parent: Some(parent),
                    name: Some(name.to_os_string()),
                    volume: id.VolumeSerialNumber,
                    id: id.FileId.Identifier,
                });
                parent = child;
            }
            let leaf = leaf.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact leaf is empty",
                )
            })?;
            let capability = Self {
                root_path,
                directories,
                parent,
                leaf,
            };
            capability.revalidate()?;
            Ok(capability)
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            let root = open_root_exact(&self.root_path)?;
            let root_id = query::<FILE_ID_INFO>(&root, FileIdInfo)?;
            let expected = &self.directories[0];
            if root_id.VolumeSerialNumber != expected.volume
                || root_id.FileId.Identifier != expected.id
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact root changed",
                ));
            }
            let held_root = query::<FILE_ID_INFO>(&expected.handle, FileIdInfo)?;
            if held_root.VolumeSerialNumber != expected.volume
                || held_root.FileId.Identifier != expected.id
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact held root changed",
                ));
            }
            for directory in self.directories.iter().skip(1) {
                let current = open_relative(
                    directory.parent.as_ref().expect("child has parent"),
                    directory.name.as_ref().expect("child has name"),
                    Some(true),
                    FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                )?;
                require_exact_directory(&current)?;
                let current_id = query::<FILE_ID_INFO>(&current, FileIdInfo)?;
                let held_id = query::<FILE_ID_INFO>(&directory.handle, FileIdInfo)?;
                if current_id.VolumeSerialNumber != directory.volume
                    || current_id.FileId.Identifier != directory.id
                    || held_id.VolumeSerialNumber != directory.volume
                    || held_id.FileId.Identifier != directory.id
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "registered artifact ancestor changed",
                    ));
                }
            }
            Ok(())
        }

        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            match open_relative(
                &self.parent,
                &self.leaf,
                None,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
            ) {
                Ok(_) => Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            }
        }

        pub(super) fn quarantine_existing(&self) -> io::Result<()> {
            self.revalidate()?;
            let file = open_relative(
                &self.parent,
                &self.leaf,
                None,
                DELETE | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let basic = query::<FILE_BASIC_INFO>(&file, FileBasicInfo)?;
            if basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact leaf is a directory",
                ));
            }
            let source_id = query::<FILE_ID_INFO>(&file, FileIdInfo)?;
            let quarantine = OsString::from(format!(
                ".axial-quarantine-{}",
                uuid::Uuid::new_v4().simple()
            ));
            rename_relative(&file, &self.parent, &quarantine)?;
            self.revalidate()?;
            if !self.target_is_missing()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact quarantine did not vacate leaf",
                ));
            }
            let quarantined = open_relative(
                &self.parent,
                &quarantine,
                None,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let quarantined = query::<FILE_ID_INFO>(&quarantined, FileIdInfo)?;
            if quarantined.VolumeSerialNumber != source_id.VolumeSerialNumber
                || quarantined.FileId.Identifier != source_id.FileId.Identifier
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact quarantine identity changed",
                ));
            }
            Ok(())
        }

        pub(super) fn create_temp(&self) -> io::Result<Temp> {
            self.revalidate()?;
            let name = OsString::from(format!(
                ".axial-repair-{}.tmp",
                uuid::Uuid::new_v4().simple()
            ));
            let file = open_relative(
                &self.parent,
                &name,
                Some(false),
                GENERIC_READ | GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_CREATE,
            )?;
            let control = file.try_clone()?;
            let id = query::<FILE_ID_INFO>(&control, FileIdInfo)?;
            Ok(Temp {
                writer: Some(file),
                control,
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
            })
        }

        pub(super) fn remove_temp(&self, temp: Temp) {
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            unsafe {
                SetFileInformationByHandle(
                    temp.control.as_raw_handle() as HANDLE,
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                );
            }
        }

        pub(super) fn promote_temp(&self, temp: &Temp) -> io::Result<()> {
            let held = query::<FILE_ID_INFO>(&temp.control, FileIdInfo)?;
            if held.VolumeSerialNumber != temp.volume || held.FileId.Identifier != temp.id {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact temp changed",
                ));
            }
            rename_relative(&temp.control, &self.parent, &self.leaf)?;
            let current = open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let current = query::<FILE_ID_INFO>(&current, FileIdInfo)?;
            if current.VolumeSerialNumber != temp.volume || current.FileId.Identifier != temp.id {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact promoted identity changed",
                ));
            }
            Ok(())
        }

        pub(super) fn verification(&self) -> Option<Verification> {
            self.revalidate().ok()?;
            let file = open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                GENERIC_READ | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ,
                FILE_OPEN,
            )
            .ok()?;
            let basic = query::<FILE_BASIC_INFO>(&file, FileBasicInfo).ok()?;
            let id = query::<FILE_ID_INFO>(&file, FileIdInfo).ok()?;
            if basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return None;
            }
            Some(Verification {
                parent: self.parent.clone(),
                leaf: self.leaf.clone(),
                file,
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
                modified: basic.LastWriteTime,
                changed: basic.ChangeTime,
            })
        }
    }

    fn open_absolute_directory_chain(path: &Path) -> io::Result<(PathBuf, Vec<HeldDirectory>)> {
        let mut components = path.components();
        let Component::Prefix(prefix) = components.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered artifact root is empty",
            )
        })?
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered artifact root is not drive-absolute",
            ));
        };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            || components.next() != Some(Component::RootDir)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered artifact root is not a supported drive path",
            ));
        }
        let mut anchor_path = PathBuf::from(prefix.as_os_str());
        anchor_path.push(Path::new(r"\"));
        let root = Arc::new(open_root_exact(&anchor_path)?);
        let root_id = query::<FILE_ID_INFO>(&root, FileIdInfo)?;
        let mut directories = vec![HeldDirectory {
            handle: root.clone(),
            parent: None,
            name: None,
            volume: root_id.VolumeSerialNumber,
            id: root_id.FileId.Identifier,
        }];
        let mut parent = root;
        for component in components {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "registered artifact root is not normalized",
                ));
            };
            let child = Arc::new(open_relative(
                &parent,
                name,
                Some(true),
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
            )?);
            require_exact_directory(&child)?;
            let id = query::<FILE_ID_INFO>(&child, FileIdInfo)?;
            directories.push(HeldDirectory {
                handle: child.clone(),
                parent: Some(parent),
                name: Some(name.to_os_string()),
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
            });
            parent = child;
        }
        Ok((anchor_path, directories))
    }

    impl Verification {
        pub(super) fn verify(mut self, expected_sha1: &str, expected_size: u64) -> bool {
            let standard = match query::<FILE_STANDARD_INFO>(&self.file, FileStandardInfo) {
                Ok(standard)
                    if !standard.Directory && standard.EndOfFile == expected_size as i64 =>
                {
                    standard
                }
                _ => return false,
            };
            let mut hasher = Sha1::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = match self.file.read(&mut buffer) {
                    Ok(count) => count,
                    Err(_) => return false,
                };
                if count == 0 {
                    break;
                }
                observed = match observed.checked_add(count as u64) {
                    Some(observed) if observed <= expected_size => observed,
                    _ => return false,
                };
                hasher.update(&buffer[..count]);
            }
            let basic = match query::<FILE_BASIC_INFO>(&self.file, FileBasicInfo) {
                Ok(basic) => basic,
                Err(_) => return false,
            };
            let held_id = match query::<FILE_ID_INFO>(&self.file, FileIdInfo) {
                Ok(id) => id,
                Err(_) => return false,
            };
            if observed != expected_size
                || standard.EndOfFile != expected_size as i64
                || basic.LastWriteTime != self.modified
                || basic.ChangeTime != self.changed
                || held_id.VolumeSerialNumber != self.volume
                || held_id.FileId.Identifier != self.id
                || format!("{:x}", hasher.finalize()) != expected_sha1
            {
                return false;
            }
            let current = match open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ,
                FILE_OPEN,
            ) {
                Ok(current) => current,
                Err(_) => return false,
            };
            query::<FILE_ID_INFO>(&current, FileIdInfo).is_ok_and(|current| {
                current.VolumeSerialNumber == self.volume && current.FileId.Identifier == self.id
            })
        }
    }

    fn query<T: Default>(file: &fs::File, class: i32) -> io::Result<T> {
        let mut value = T::default();
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                class,
                (&mut value as *mut T).cast(),
                size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    fn open_root_exact(root: &std::path::Path) -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options
            .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        let file = options.open(root)?;
        require_exact_directory(&file)?;
        Ok(file)
    }

    fn require_exact_directory(file: &fs::File) -> io::Result<()> {
        let basic = query::<FILE_BASIC_INFO>(file, FileBasicInfo)?;
        let standard = query::<FILE_STANDARD_INFO>(file, FileStandardInfo)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || !standard.Directory
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered artifact ancestor is not an exact directory",
            ));
        }
        Ok(())
    }

    fn open_relative(
        parent: &fs::File,
        name: &OsStr,
        directory: Option<bool>,
        access: u32,
        share: u32,
        disposition: u32,
    ) -> io::Result<fs::File> {
        let mut encoded = name.encode_wide().collect::<Vec<_>>();
        if encoded.is_empty() || encoded.len() > (u16::MAX as usize / 2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid relative leaf",
            ));
        }
        let mut unicode = UNICODE_STRING {
            Length: (encoded.len() * 2) as u16,
            MaximumLength: (encoded.len() * 2) as u16,
            Buffer: encoded.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle() as HANDLE,
            ObjectName: &mut unicode,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        let mut status = IO_STATUS_BLOCK::default();
        let mut handle: HANDLE = ptr::null_mut();
        let type_option = match directory {
            Some(true) => FILE_DIRECTORY_FILE,
            Some(false) => FILE_NON_DIRECTORY_FILE,
            None => 0,
        };
        let result = unsafe {
            NtCreateFile(
                &mut handle,
                access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut status,
                ptr::null(),
                0,
                share,
                disposition,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | type_option,
                ptr::null(),
                0,
            )
        };
        if result < 0 {
            if !handle.is_null() {
                unsafe { CloseHandle(handle) };
            }
            let code = unsafe { RtlNtStatusToDosError(result) };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(unsafe { fs::File::from_raw_handle(handle) })
    }

    fn rename_relative(file: &fs::File, parent: &fs::File, name: &OsStr) -> io::Result<()> {
        let encoded = name.encode_wide().collect::<Vec<_>>();
        let name_bytes = encoded
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or_else(|| io::Error::other("rename target too long"))?;
        let buffer_size = size_of::<FILE_RENAME_INFORMATION>()
            .checked_add(name_bytes)
            .ok_or_else(|| io::Error::other("rename buffer overflow"))?;
        let mut buffer = vec![0_usize; buffer_size.div_ceil(size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = parent.as_raw_handle() as HANDLE;
            (*info).FileNameLength = name_bytes
                .try_into()
                .map_err(|_| io::Error::other("rename target too long"))?;
            ptr::copy_nonoverlapping(
                encoded.as_ptr(),
                (*info).FileName.as_mut_ptr(),
                encoded.len(),
            );
            let mut status = IO_STATUS_BLOCK::default();
            let result = NtSetInformationFile(
                file.as_raw_handle() as HANDLE,
                &mut status,
                info.cast(),
                buffer_size
                    .try_into()
                    .map_err(|_| io::Error::other("rename buffer too large"))?,
                FileRenameInformation,
            );
            if result < 0 {
                let code = RtlNtStatusToDosError(result);
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::RegisteredArtifactMutationCapability;
    use crate::state::contracts::{
        OperationId, OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    #[tokio::test]
    async fn ancestor_swap_cannot_redirect_quarantine_outside_the_held_root() {
        let base = std::env::temp_dir().join(format!(
            "axial-registered-artifact-confinement-{}",
            uuid::Uuid::new_v4()
        ));
        let managed_root = base.join("managed");
        let detached_root = base.join("detached-managed");
        let outside_root = base.join("outside");
        let relative = PathBuf::from("libraries/example/leaf.jar");
        fs::create_dir_all(managed_root.join("libraries/example"))
            .expect("create managed artifact parent");
        fs::create_dir_all(outside_root.join("libraries/example"))
            .expect("create outside artifact parent");
        fs::write(managed_root.join(&relative), b"managed-corrupt")
            .expect("write managed artifact");
        fs::write(outside_root.join(&relative), b"user-owned").expect("write outside artifact");

        let capability = RegisteredArtifactMutationCapability::mint(
            KnownGoodPhysicalPath::for_test(managed_root.clone(), relative.clone()),
        )
        .await
        .expect("mint confined mutation capability");
        fs::rename(&managed_root, &detached_root).expect("detach held managed root");
        symlink(&outside_root, &managed_root).expect("redirect configured root");

        let result = capability.quarantine_existing(
            &OperationId::new("registered-artifact-confinement-test"),
            &TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "registered-artifact-confinement-test",
                OwnershipClass::LauncherManaged,
            ),
        );

        assert!(result.is_err(), "ancestor drift must fail closed");
        assert_eq!(
            fs::read(outside_root.join(&relative)).expect("read outside artifact"),
            b"user-owned",
            "outside artifact must remain untouched"
        );
        assert_eq!(
            fs::read(detached_root.join(&relative)).expect("read held managed artifact"),
            b"managed-corrupt",
            "failed confinement must not mutate the detached managed artifact"
        );
        assert!(
            fs::read_dir(outside_root.join("libraries/example"))
                .expect("read outside artifact parent")
                .all(|entry| !entry
                    .expect("outside directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-quarantine-")),
            "quarantine must not be redirected outside the held namespace"
        );

        fs::remove_file(&managed_root).expect("remove redirected root link");
        fs::remove_dir_all(&base).expect("remove confinement fixture");
    }
}
