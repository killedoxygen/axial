//! Private adapters around the retained capability filesystem.
//!
//! A spelling is never authority. Entry access starts from a generation-pinned
//! directory and the leaf implementation validates handles, ancestry and names.
//! Native effect outcomes are deliberately retained in full: callers must keep
//! their returned generation pin with every escaped capability or obligation.

mod park;
pub(crate) mod portable;

pub(crate) use park::{DirectoryParkReceipt, RecoveredDirectoryPark};
pub(crate) use portable::PortableFileName;
pub(crate) use portable::{PortableFileName as PortableName, PortableRelativePath as ScopedPath};

use crate::library::GenerationPin;
use axial_fs::{
    Directory, DirectoryIdentity, DirectoryRevision, FileCapability, FileRevision, LeafName,
};
use std::io;

/// A native result and the physical library generation needed to settle it.
/// Extracting parts transfers both obligations to the feature owner.
#[must_use = "retain the generation alongside every native capability and unresolved effect"]
pub(crate) struct Retained<T> {
    value: T,
    pin: GenerationPin,
}

impl<T> Retained<T> {
    pub(crate) fn new(value: T, pin: GenerationPin) -> Self {
        Self { value, pin }
    }

    pub(crate) fn into_parts(self) -> (T, GenerationPin) {
        (self.value, self.pin)
    }

    pub(crate) fn value(&self) -> &T {
        &self.value
    }
    pub(crate) fn pin(&self) -> &GenerationPin {
        &self.pin
    }

    pub(crate) fn map<U>(self, operation: impl FnOnce(T) -> U) -> Retained<U> {
        Retained::new(operation(self.value), self.pin)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Retained<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retained")
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ScopedDirectory {
    directory: Directory,
    pin: GenerationPin,
}

impl ScopedDirectory {
    /// Only lifecycle/feature owners may pair an already admitted capability
    /// with its pin. This constructor does not open or infer a root from a path.
    pub(crate) fn from_admitted(directory: Directory, pin: GenerationPin) -> io::Result<Self> {
        pin.revalidate()?;
        directory.validate_within(&pin.directory()?)?;
        Ok(Self { directory, pin })
    }

    pub(crate) fn pin(&self) -> &GenerationPin {
        &self.pin
    }

    /// The caller must retain this scope while borrowing the native capability,
    /// and attach a cloned pin to all native effects that escape the call.
    pub(crate) fn capability(&self) -> &Directory {
        &self.directory
    }

    pub(crate) fn revalidate(&self) -> io::Result<()> {
        self.pin.revalidate()?;
        self.directory.identity().map(|_| ())
    }

    pub(crate) fn read_projection(&self) -> io::Result<std::path::PathBuf> {
        self.revalidate()?;
        let root = self.pin.files()?;
        self.directory
            .project_from(&root.directory, &self.pin.read_projection()?)
    }

    pub(crate) fn identity(&self) -> io::Result<DirectoryIdentity> {
        self.directory.identity()
    }
    pub(crate) fn revision(&self) -> io::Result<DirectoryRevision> {
        self.directory.revision()
    }
    pub(crate) fn validate_revision(&self, revision: &DirectoryRevision) -> io::Result<()> {
        self.directory.validate_revision(revision)
    }

    /// A bounded equality witness, not a serialized filesystem capability.
    /// The directory witness is rename-stable; the root witness binds its scope.
    pub(crate) fn receipt(&self) -> io::Result<String> {
        self.revalidate()?;
        let root = self.pin.files()?.directory.identity()?.filesystem_witness();
        let directory = self.directory.identity()?.filesystem_witness();
        Ok(format!(
            "axial-dir-v1:{}:{}",
            hex::encode(root),
            hex::encode(directory)
        ))
    }

    pub(crate) fn matches_receipt(&self, expected: &str) -> io::Result<bool> {
        if expected.len() != 142 || !expected.starts_with("axial-dir-v1:") {
            return Err(invalid("directory receipt is invalid"));
        }
        Ok(self.receipt()? == expected)
    }

    pub(crate) fn verify_receipt(&self, expected: &str) -> io::Result<()> {
        if self.matches_receipt(expected)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory receipt no longer matches",
            ))
        }
    }

    pub(crate) fn entries(&self, limit: usize) -> io::Result<axial_fs::DirectoryListing> {
        self.revalidate()?;
        self.directory.entries(limit)
    }

    pub(crate) fn open_directory(&self, name: &PortableName) -> io::Result<Self> {
        Self::from_admitted(
            self.directory.open_directory(&leaf(name))?,
            self.pin.clone(),
        )
    }

    pub(crate) fn directory(&self, path: &ScopedPath) -> io::Result<Self> {
        let mut directory = self.clone();
        for component in path.as_str().split('/') {
            directory = directory.open_directory(
                &PortableName::new_exact(component)
                    .map_err(|_| invalid("scoped path contains an invalid name"))?,
            )?;
        }
        Ok(directory)
    }

    pub(crate) fn open_file(&self, name: &PortableName) -> io::Result<ScopedFile> {
        ScopedFile::from_admitted(self.directory.open_file(&leaf(name))?, self.pin.clone())
    }

    pub(crate) fn read_bounded(&self, path: &ScopedPath, max_bytes: u64) -> io::Result<Vec<u8>> {
        let (parent, name) = self.parent_and_name(path)?;
        parent.open_file(&name)?.read_bounded(max_bytes)
    }

    pub(crate) fn create_directory(
        &self,
        name: &PortableName,
    ) -> Retained<axial_fs::DirectoryCreateOutcome> {
        Retained::new(
            self.directory.create_directory(&leaf(name)),
            self.pin.clone(),
        )
    }

    pub(crate) fn move_no_replace(
        self,
        destination: &Self,
        name: &PortableName,
    ) -> Retained<axial_fs::DirectoryMoveOutcome> {
        Retained::new(
            self.directory
                .move_no_replace(&destination.directory, &leaf(name)),
            self.pin,
        )
    }

    pub(crate) fn park(self) -> Retained<axial_fs::DirectoryParkOutcome> {
        Retained::new(self.directory.park(), self.pin)
    }

    /// Durably records a create-only destination before producing a stage.
    /// A crash may replay the leaf's publication journal; the owning domain must
    /// persist its intent before calling this and commit metadata after publish.
    pub(crate) fn stage(&self, name: &PortableName) -> StageCreateOutcome {
        match self.directory.create_recoverable_stage(&leaf(name)) {
            axial_fs::FileCreateOutcome::Created(stage) => {
                StageCreateOutcome::Created(StagedFile {
                    stage,
                    parent: self.clone(),
                    destination: name.clone(),
                    written: 0,
                    started: false,
                })
            }
            axial_fs::FileCreateOutcome::NoEffect(error) => StageCreateOutcome::NoEffect(error),
            axial_fs::FileCreateOutcome::AppliedUnverified(obligation) => {
                StageCreateOutcome::Unresolved {
                    obligation: Retained::new(obligation, self.pin.clone()),
                    parent: self.clone(),
                    destination: name.clone(),
                }
            }
        }
    }

    fn parent_and_name(&self, path: &ScopedPath) -> io::Result<(Self, PortableName)> {
        match path.as_str().rsplit_once('/') {
            Some((parent, name)) => Ok((
                self.directory(
                    &ScopedPath::new_exact(parent).map_err(|_| invalid("invalid scoped parent"))?,
                )?,
                PortableName::new_exact(name).map_err(|_| invalid("invalid scoped name"))?,
            )),
            None => Ok((self.clone(), path.file_name())),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ScopedFile {
    file: FileCapability,
    pin: GenerationPin,
}

impl ScopedFile {
    pub(crate) fn from_admitted(file: FileCapability, pin: GenerationPin) -> io::Result<Self> {
        pin.revalidate()?;
        file.validate_within(&pin.directory()?)?;
        Ok(Self { file, pin })
    }

    pub(crate) fn capability(&self) -> &FileCapability {
        &self.file
    }
    pub(crate) fn pin(&self) -> &GenerationPin {
        &self.pin
    }
    pub(crate) fn revision(&self) -> io::Result<FileRevision> {
        self.file.revision()
    }
    pub(crate) fn validate_revision(&self, revision: &FileRevision) -> io::Result<()> {
        self.file.validate_revision(revision)
    }
    pub(crate) fn read_bounded(&self, max_bytes: u64) -> io::Result<Vec<u8>> {
        self.file.read_bounded(max_bytes)
    }
    pub(crate) fn reader(&self, max_bytes: u64) -> io::Result<axial_fs::FileReader<'_>> {
        self.file.reader(max_bytes)
    }

    pub(crate) fn move_no_replace(
        self,
        destination: &ScopedDirectory,
        name: &PortableName,
    ) -> Retained<axial_fs::FileMoveOutcome> {
        Retained::new(
            self.file
                .move_no_replace(&destination.directory, &leaf(name)),
            self.pin,
        )
    }

    pub(crate) fn park(
        self,
        expected: axial_fs::ExpectedFileContent,
        parent: &ScopedDirectory,
    ) -> Retained<axial_fs::FileParkOutcome> {
        Retained::new(
            parent.directory.park_file(self.file.park_request(expected)),
            self.pin,
        )
    }
}

#[must_use = "staging effects must be retained or settled"]
#[derive(Debug)]
pub(crate) enum StageCreateOutcome {
    Created(StagedFile),
    NoEffect(io::Error),
    Unresolved {
        obligation: Retained<axial_fs::FileCreateObligation>,
        parent: ScopedDirectory,
        destination: PortableName,
    },
}

#[must_use = "a stage must be sealed, discarded, or retained"]
#[derive(Debug)]
pub(crate) struct StagedFile {
    stage: axial_fs::StagedFile,
    parent: ScopedDirectory,
    destination: PortableName,
    written: u64,
    started: bool,
}

impl StagedFile {
    /// Write the complete payload once. The retained leaf writer truncates when
    /// opened, so repeated chunk calls must never masquerade as append writes.
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.started {
            return Err(invalid(
                "stage payload is already written; use one writer for streaming",
            ));
        }
        let written = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("stage byte count overflow"))?;
        self.started = true;
        self.stage.write_all(bytes)?;
        self.written = written;
        Ok(())
    }

    pub(crate) fn copy_from(&mut self, reader: &mut impl io::Read, limit: u64) -> io::Result<u64> {
        if self.started {
            return Err(invalid("stage payload is already written"));
        }
        use io::Write;
        self.started = true;
        let mut writer = self.stage.writer()?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let written = self
                .written
                .checked_add(read as u64)
                .filter(|written| *written <= limit)
                .ok_or_else(|| invalid("stage exceeds its byte bound"))?;
            writer.write_all(&buffer[..read])?;
            self.written = written;
        }
        writer.finish()?;
        Ok(self.written)
    }

    /// Open exactly one retained writer for a streaming producer. Its caller
    /// owns byte bounds and calls finish before sealing the stage.
    pub(crate) fn writer(&mut self) -> io::Result<axial_fs::StagedWriter<'_>> {
        if self.started {
            return Err(invalid("stage payload is already written"));
        }
        self.started = true;
        self.stage.writer()
    }

    pub(crate) fn write_bounded(&mut self, bytes: &[u8], limit: u64) -> io::Result<()> {
        if self
            .written
            .checked_add(bytes.len() as u64)
            .is_none_or(|n| n > limit)
        {
            return Err(invalid("stage exceeds its byte bound"));
        }
        self.write_all(bytes)
    }

    pub(crate) fn seal(self) -> Result<SealedFile, StageSealFailure> {
        match self.stage.seal() {
            Ok(stage) => Ok(SealedFile {
                stage,
                parent: self.parent,
                destination: self.destination,
            }),
            Err(error) => Err(StageSealFailure {
                error,
                parent: self.parent,
                destination: self.destination,
                written: self.written,
                started: self.started,
            }),
        }
    }

    pub(crate) fn discard(self) -> Retained<axial_fs::StageDiscardOutcome> {
        Retained::new(self.stage.discard(), self.parent.pin)
    }
}

#[derive(Debug)]
pub(crate) struct StageSealFailure {
    error: axial_fs::StageSealFailure,
    parent: ScopedDirectory,
    destination: PortableName,
    written: u64,
    started: bool,
}

impl StageSealFailure {
    pub(crate) fn error(&self) -> &io::Error {
        self.error.error()
    }
    pub(crate) fn into_staged(self) -> StagedFile {
        StagedFile {
            stage: self.error.into_staged(),
            parent: self.parent,
            destination: self.destination,
            written: self.written,
            started: self.started,
        }
    }
}

#[must_use = "a sealed stage must be published, discarded, or retained"]
#[derive(Debug)]
pub(crate) struct SealedFile {
    stage: axial_fs::SealedStagedFile,
    parent: ScopedDirectory,
    destination: PortableName,
}

impl SealedFile {
    pub(crate) fn publish(self) -> Retained<axial_fs::FilePromotionOutcome> {
        Retained::new(
            self.stage.promote_no_replace(
                &self.parent.directory,
                &self.parent.directory,
                &leaf(&self.destination),
            ),
            self.parent.pin,
        )
    }
    pub(crate) fn discard(self) -> Retained<axial_fs::StageDiscardOutcome> {
        Retained::new(self.stage.discard(), self.parent.pin)
    }
}

fn leaf(name: &PortableName) -> LeafName {
    LeafName::new(name.as_str()).expect("portable names are valid capability leaves")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests;
