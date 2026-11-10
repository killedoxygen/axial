use super::*;

pub struct FileCapability {
    pub(crate) handle: File,
    pub(crate) identity: platform::Identity,
    pub(crate) parent: Directory,
    pub(crate) name: LeafName,
    pub(crate) authority: Weak<CapabilityAuthority>,
}

pub struct FileRevision {
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) stamp: platform::FileStamp,
}

#[derive(Clone)]
pub struct FileRevisionObservation {
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) stamp: platform::FileStamp,
}

impl PartialEq for FileRevisionObservation {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.authority, &other.authority)
            && self.identity == other.identity
            && self.size == other.size
            && self.stamp == other.stamp
    }
}

impl Eq for FileRevisionObservation {}

impl FileRevisionObservation {
    /// Durable comparison evidence for this exact revision, never filesystem authority.
    /// Matches `FileCapability::revision_witness` for the same physical revision.
    pub fn witness(&self) -> io::Result<[u8; 32]> {
        revision_witness(self.identity, self.size, self.stamp)
    }
}

pub(crate) fn revision_witness(
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
) -> io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};

    let modified_at_ns = platform::file_modified_at_ns(stamp)
        .map_err(|_| io::Error::from(io::ErrorKind::Unsupported))?;
    let changed_at_ns = platform::file_changed_at_ns(stamp)
        .map_err(|_| io::Error::from(io::ErrorKind::Unsupported))?;
    let mut digest = Sha256::new();
    digest.update(b"axial-fs-file-revision-v1");
    digest.update(platform::identity_witness(identity));
    digest.update(size.to_le_bytes());
    digest.update(modified_at_ns.to_le_bytes());
    digest.update(changed_at_ns.to_le_bytes());
    Ok(digest.finalize().into())
}

impl std::hash::Hash for FileRevisionObservation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.authority.as_ptr().hash(state);
        self.identity.hash(state);
        self.size.hash(state);
    }
}

#[must_use = "file revision readers must be explicitly finished or cancelled"]
pub struct FileRevisionReader {
    pub(crate) state: Option<FileRevisionReaderState>,
}

pub(crate) struct FileRevisionReaderState {
    pub(crate) file: FileCapability,
    pub(crate) expected: FileRevision,
    pub(crate) position: u64,
    pub(crate) operation: CapabilityOperation,
}

impl_redacted_debug!(FileRevisionReader);

#[must_use = "file revision reader start failures retain the file and revision and must be retried or unpacked"]
pub struct FileRevisionReaderStartFailure {
    pub(crate) error: Option<io::Error>,
    pub(crate) file: Option<FileCapability>,
    pub(crate) expected: Option<FileRevision>,
    pub(crate) max_bytes: u64,
}

impl_redacted_debug!(FileRevisionReaderStartFailure);

impl FileRevisionReaderStartFailure {
    pub(crate) fn new(
        error: io::Error,
        file: FileCapability,
        expected: FileRevision,
        max_bytes: u64,
    ) -> Self {
        Self {
            error: Some(error),
            file: Some(file),
            expected: Some(expected),
            max_bytes,
        }
    }

    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("reader start failure retains its error")
    }

    pub fn retry(mut self) -> Result<FileRevisionReader, Self> {
        let file = self
            .file
            .take()
            .expect("reader start failure retains its file");
        let expected = self
            .expected
            .take()
            .expect("reader start failure retains its revision");
        file.into_revision_reader(expected, self.max_bytes)
    }

    pub fn into_parts(mut self) -> (io::Error, FileCapability, FileRevision, u64) {
        let error = self
            .error
            .take()
            .expect("reader start failure retains its error");
        let file = self
            .file
            .take()
            .expect("reader start failure retains its file");
        let expected = self
            .expected
            .take()
            .expect("reader start failure retains its revision");
        (error, file, expected, self.max_bytes)
    }
}

impl Drop for FileRevisionReaderStartFailure {
    fn drop(&mut self) {
        if self.file.is_some() || self.expected.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "file revision reader finish failures retain the armed reader and must be retried or unpacked"]
pub struct FileRevisionReaderFinishFailure {
    pub(crate) error: Option<io::Error>,
    pub(crate) reader: Option<FileRevisionReader>,
}

impl_redacted_debug!(FileRevisionReaderFinishFailure);

impl FileRevisionReaderFinishFailure {
    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("reader finish failure retains its error")
    }

    pub fn retry(mut self) -> Result<FileCapability, Self> {
        self.reader
            .take()
            .expect("reader finish failure retains its reader")
            .finish()
    }

    pub fn into_reader(mut self) -> FileRevisionReader {
        self.reader
            .take()
            .expect("reader finish failure retains its reader")
    }
}

impl Drop for FileRevisionReaderFinishFailure {
    fn drop(&mut self) {
        if self.reader.is_some() {
            std::process::abort();
        }
    }
}

impl_redacted_debug!(FileRevision);

impl_redacted_debug!(FileRevisionObservation);

impl FileRevision {
    pub fn retained(&self) -> Self {
        Self {
            authority: self.authority.clone(),
            identity: self.identity,
            size: self.size,
            stamp: self.stamp,
        }
    }

    pub fn observation(&self) -> FileRevisionObservation {
        FileRevisionObservation {
            authority: self.authority.clone(),
            identity: self.identity,
            size: self.size,
            stamp: self.stamp,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified_at_ns(&self) -> io::Result<u64> {
        platform::file_modified_at_ns(self.stamp)
    }

    pub fn has_same_rename_stable_metadata(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.authority, &other.authority)
            && self.identity == other.identity
            && self.size == other.size
            && platform::file_content_stamp_matches(self.stamp, other.stamp)
    }
}

pub struct ExpectedFileContent {
    pub(crate) revision: FileRevision,
    pub(crate) sha256: [u8; 32],
}

impl_redacted_debug!(ExpectedFileContent);

impl ExpectedFileContent {
    pub fn new(revision: FileRevision, sha256: [u8; 32]) -> Self {
        Self { revision, sha256 }
    }
}

pub struct FileParkRequest {
    pub(crate) file: FileCapability,
    pub(crate) expected: ExpectedFileContent,
}

impl_redacted_debug!(FileParkRequest);

impl FileParkRequest {
    pub fn into_parts(self) -> (FileCapability, FileRevision, [u8; 32]) {
        (self.file, self.expected.revision, self.expected.sha256)
    }

    pub(crate) fn validate_revision(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.file
            .validate_revision_in(operation, &self.expected.revision)
    }

    pub(crate) fn validate_authority_after_namespace_change(
        &self,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        self.file.parent.validate(operation)?;
        if self.file.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.expected.revision.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || platform::file_identity(&self.file.handle)? != self.file.identity
            || self.expected.revision.identity != self.file.identity
        {
            return Err(identity_changed(
                "file authority changed across its namespace transition",
            ));
        }
        Ok(())
    }
}

impl_redacted_debug!(FileCapability);

impl FileCapability {
    pub fn validate_within(&self, anchor: &Directory) -> io::Result<()> {
        self.parent.validate_within(anchor)?;
        self.revision().map(|_| ())
    }

    #[cfg(unix)]
    pub fn make_executable(&self) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        platform::make_file_executable(&self.handle)?;
        self.validate(&operation)
    }

    #[cfg(unix)]
    pub fn is_executable(&self) -> io::Result<bool> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        let executable = platform::file_is_executable(&self.handle)?;
        self.validate(&operation)?;
        Ok(executable)
    }

    /// A negative filter using captured identities, not validation or authority.
    /// A possible match must still be verified with `same_file`.
    pub fn could_be_same_file(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.authority, &other.authority) && self.identity == other.identity
    }

    /// Hashes exactly the evidence compared by `could_be_same_file`.
    pub fn hash_captured_identity(&self, hasher: &mut impl std::hash::Hasher) {
        use std::hash::Hash;
        self.authority.as_ptr().hash(hasher);
        self.identity.hash(hasher);
    }

    pub fn same_file(&self, other: &Self) -> io::Result<bool> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        other.validate(&operation)?;
        if !Weak::ptr_eq(&self.authority, &other.authority) {
            return Err(stale_capability());
        }
        Ok(self.identity == other.identity)
    }

    pub fn move_no_replace(
        self,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> FileMoveOutcome {
        self.move_no_replace_internal(destination, destination_name, true)
    }

    /// Like `move_no_replace`, but an applied move is not yet durable: the caller must
    /// `Directory::sync` both parents before treating it as durable. Batched publication
    /// uses this to sync each touched directory once instead of after every move.
    pub fn move_no_replace_deferring_parent_sync(
        self,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> FileMoveOutcome {
        self.move_no_replace_internal(destination, destination_name, false)
    }

    pub(crate) fn move_no_replace_internal(
        self,
        destination: &Directory,
        destination_name: &LeafName,
        sync_parents: bool,
    ) -> FileMoveOutcome {
        let source_parent = self.parent.clone();
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => return FileMoveOutcome::NoEffect { error, item: self },
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileMoveOutcome::NoEffect { error, item: self },
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| destination.validate(&operation))
        {
            return FileMoveOutcome::NoEffect { error, item: self };
        }
        if !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority) {
            return FileMoveOutcome::NoEffect {
                error: stale_capability(),
                item: self,
            };
        }
        if source_parent.inner.identity == destination.inner.identity
            && platform::leaf_names_equal(self.name.as_os_str(), destination_name.as_os_str())
        {
            return FileMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file move destination matches its source",
                ),
                item: self,
            };
        }
        let mut token = match MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: source_parent.clone(),
                name: self.name.clone(),
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            None,
            Some(self.identity),
        ) {
            Ok(token) => token,
            Err(error) => return FileMoveOutcome::NoEffect { error, item: self },
        };
        let effect = platform::move_file_no_replace(
            source_parent.inner.handle(),
            self.name.as_os_str(),
            &self.handle,
            destination.inner.handle(),
            destination_name.as_os_str(),
        );
        let reported_success = effect.is_ok();
        match settle_file_move(
            self,
            destination,
            destination_name,
            reported_success,
            &mut token,
            sync_parents,
        ) {
            Ok((true, file)) => FileMoveOutcome::Applied(file),
            Ok((false, file)) => FileMoveOutcome::NoEffect {
                error: effect.err().unwrap_or_else(|| {
                    io::Error::other("file move reported no effect after native success")
                }),
                item: file,
            },
            Err(file) => FileMoveOutcome::AppliedUnverified(FileMoveObligation {
                error: effect
                    .err()
                    .unwrap_or_else(|| io::Error::other("file move could not be classified")),
                file: Some(file),
                destination: destination.clone(),
                destination_name: destination_name.clone(),
                reported_success,
                token,
            }),
        }
    }

    pub fn revision(&self) -> io::Result<FileRevision> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        let (size, stamp) = platform::file_receipt_fields(&self.handle)?;
        self.validate(&operation)?;
        Ok(FileRevision {
            authority: self.authority.clone(),
            identity: self.identity,
            size,
            stamp,
        })
    }

    /// Comparison evidence for a freshly admitted file, never filesystem authority.
    /// Includes physical identity, size and revision, but not the root session.
    pub fn revision_witness(&self) -> io::Result<[u8; 32]> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let (size, stamp) = platform::file_receipt_fields(&self.handle)?;
        let witness = revision_witness(self.identity, size, stamp)?;
        if platform::file_receipt_fields(&self.handle)? != (size, stamp) {
            return Err(identity_changed("file revision changed"));
        }
        self.validate(&operation)?;
        Ok(witness)
    }

    pub fn park_request(self, expected: ExpectedFileContent) -> FileParkRequest {
        FileParkRequest {
            file: self,
            expected,
        }
    }

    pub fn validate_revision(&self, expected: &FileRevision) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)
    }

    pub fn validate_revision_observation(
        &self,
        expected: &FileRevisionObservation,
    ) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate_revision_observation_in(&operation, expected)
    }

    pub(crate) fn validate_revision_observation_in(
        &self,
        operation: &CapabilityOperation,
        expected: &FileRevisionObservation,
    ) -> io::Result<()> {
        self.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let receipt = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || receipt != (expected.size, expected.stamp)
        {
            return Err(identity_changed("file revision observation changed"));
        }
        self.validate(operation)
    }

    pub(crate) fn validate_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &FileRevision,
    ) -> io::Result<()> {
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let receipt = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || receipt != (expected.size, expected.stamp)
        {
            return Err(identity_changed("file revision changed"));
        }
        Ok(())
    }

    pub(crate) fn validate_content_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &FileRevision,
    ) -> io::Result<()> {
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let (size, stamp) = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || expected.size != size
            || !platform::file_content_stamp_matches(expected.stamp, stamp)
        {
            return Err(identity_changed("file content revision changed"));
        }
        Ok(())
    }

    pub fn read_range_bounded(
        &self,
        expected: &FileRevision,
        offset: u64,
        length: usize,
    ) -> io::Result<Vec<u8>> {
        if length > MAX_FILE_RANGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file range exceeds the supported bound",
            ));
        }
        let length_u64 = u64::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file range is too large"))?;
        let end = offset
            .checked_add(length_u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file range overflowed"))?;
        if end > expected.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file range exceeds its expected revision",
            ));
        }

        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        let mut bytes = vec![0_u8; length];
        let mut cursor = offset;
        let mut written = 0_usize;
        while cursor < end {
            let read = platform::read_at(&self.handle, &mut bytes[written..], cursor)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before the requested range completed",
                ));
            }
            cursor = cursor
                .checked_add(u64::try_from(read).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "file read count is too large")
                })?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "file cursor overflowed")
                })?;
            written = written.checked_add(read).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "file result length overflowed")
            })?;
        }
        if cursor != end || bytes.len() != length || written != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file range did not complete exactly",
            ));
        }
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)?;
        Ok(bytes)
    }

    pub fn into_revision_reader(
        self,
        expected: FileRevision,
        max_bytes: u64,
    ) -> Result<FileRevisionReader, FileRevisionReaderStartFailure> {
        if expected.size > max_bytes {
            return Err(FileRevisionReaderStartFailure::new(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file revision exceeds its reader bound",
                ),
                self,
                expected,
                max_bytes,
            ));
        }
        let authority = match self.parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return Err(FileRevisionReaderStartFailure::new(
                    error, self, expected, max_bytes,
                ));
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return Err(FileRevisionReaderStartFailure::new(
                    error, self, expected, max_bytes,
                ));
            }
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| self.validate_revision_in(&operation, &expected))
        {
            drop(operation);
            return Err(FileRevisionReaderStartFailure::new(
                error, self, expected, max_bytes,
            ));
        }
        Ok(FileRevisionReader {
            state: Some(FileRevisionReaderState {
                file: self,
                expected,
                position: 0,
                operation,
            }),
        })
    }

    pub fn reader(&self, max_bytes: u64) -> io::Result<FileReader<'_>> {
        let authority = self.parent.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        Ok(FileReader {
            file: self,
            operation,
            position: 0,
            max_bytes,
        })
    }

    pub fn read_bounded(&self, max_bytes: u64) -> io::Result<Vec<u8>> {
        let mut reader = self.reader(max_bytes)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        reader.finish()?;
        Ok(bytes)
    }

    pub(crate) fn new(
        handle: File,
        identity: platform::Identity,
        parent: Directory,
        name: LeafName,
        authority: Weak<CapabilityAuthority>,
    ) -> Self {
        Self {
            handle,
            identity,
            parent,
            name,
            authority,
        }
    }

    pub(crate) fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        // An observation defers a remembered file to the window's revalidation
        // of its single-link binding.
        let remembered =
            operation.windowed && self.observe_remembered_binding(&operation.authority);
        if remembered && operation.remembers_files {
            return Ok(());
        }
        if platform::file_identity(&self.handle)? != self.identity {
            return Err(identity_changed("file capability changed binding"));
        }
        if remembered {
            return Ok(());
        }
        if platform::file_binding_state(
            self.parent.inner.handle(),
            self.name.as_os_str(),
            self.identity,
        )? != platform::BindingState::Exact
        {
            return Err(identity_changed("file capability changed binding"));
        }
        if operation.remembers_files {
            self.remember_binding(&operation.authority)?;
        }
        Ok(())
    }

    /// Relies on an exact binding this thread's window remembers, if any.
    pub(crate) fn observe_remembered_binding(&self, authority: &Arc<CapabilityAuthority>) -> bool {
        with_observation_window(authority, |window| {
            let remembered = window.files.iter_mut().find(|file| {
                Arc::ptr_eq(&file.parent, &self.parent.inner)
                    && file.identity == self.identity
                    && file.name == self.name
            });
            remembered.map(|file| file.observed = true).is_some()
        })
        .unwrap_or(false)
    }

    /// Returns whether the window remembers the binding; it does not during effects.
    pub(crate) fn remember_binding(
        &self,
        authority: &Arc<CapabilityAuthority>,
    ) -> io::Result<bool> {
        if self.observe_remembered_binding(authority) {
            return Ok(true);
        }
        let binding = || ObservedFile {
            parent: self.parent.inner.clone(),
            name: self.name.clone(),
            identity: self.identity,
            observed: true,
        };
        let full = with_observation_window(authority, |window| {
            // Remembered bindings pin only parents the bounded directory cache pins.
            if window.effects != 0 || window.position(&self.parent.inner).is_none() {
                return ControlFlow::Break(false);
            }
            if window.files.len() < MAX_OBSERVATION_WINDOW_FILES {
                window.files.push(binding());
                return ControlFlow::Break(true);
            }
            ControlFlow::Continue(std::mem::take(&mut window.files))
        });
        let full = match full {
            Some(ControlFlow::Continue(full)) => full,
            Some(ControlFlow::Break(remembered)) => return Ok(remembered),
            None => return Ok(false),
        };
        validate_window_files(full.iter().filter(|file| file.observed))
            .inspect_err(|_| fail_observation_window(authority))?;
        with_observation_window(authority, |window| window.files.push(binding()));
        Ok(true)
    }

    pub(crate) fn validate_bound_to(
        &self,
        parent: &Directory,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        self.validate(operation)?;
        parent.validate(operation)?;
        if self.parent.inner.identity == parent.inner.identity {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file capability belongs to another directory",
            ))
        }
    }
}

impl Read for FileRevisionReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let state = self
            .state
            .as_mut()
            .expect("file revision reader retains armed state");
        state.file.validate(&state.operation)?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        if bytes.is_empty() || state.position == state.expected.size {
            return Ok(0);
        }
        let remaining = state
            .expected
            .size
            .checked_sub(state.position)
            .ok_or_else(|| io::Error::other("file revision reader position overflowed"))?;
        let allowed = usize::try_from(remaining.min(bytes.len() as u64)).map_err(|_| {
            io::Error::other("file revision read length does not fit this platform")
        })?;
        let read = platform::read_at(&state.file.handle, &mut bytes[..allowed], state.position)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file ended before its admitted revision",
            ));
        }
        let position = state
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file revision reader position overflowed"))?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        state.file.validate(&state.operation)?;
        state.position = position;
        Ok(read)
    }
}

impl Seek for FileRevisionReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let state = self
            .state
            .as_mut()
            .expect("file revision reader retains armed state");
        state.file.validate(&state.operation)?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(state.expected.size) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(state.position) + i128::from(delta),
        };
        if !(0..=i128::from(state.expected.size)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file revision reader seek escaped its admitted range",
            ));
        }
        let position = u64::try_from(next)
            .map_err(|_| io::Error::other("file revision reader position overflowed"))?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        state.file.validate(&state.operation)?;
        state.position = position;
        Ok(position)
    }
}

impl FileRevisionReader {
    pub fn finish(mut self) -> Result<FileCapability, FileRevisionReaderFinishFailure> {
        let validation = {
            let state = self
                .state
                .as_ref()
                .expect("file revision reader retains armed state");
            state
                .file
                .validate(&state.operation)
                .and_then(|_| {
                    state
                        .file
                        .validate_revision_in(&state.operation, &state.expected)
                })
                .and_then(|_| state.file.validate(&state.operation))
        };
        if let Err(error) = validation {
            return Err(FileRevisionReaderFinishFailure {
                error: Some(error),
                reader: Some(self),
            });
        }
        let FileRevisionReaderState {
            file,
            expected: _,
            position: _,
            operation,
        } = self
            .state
            .take()
            .expect("file revision reader retains armed state");
        drop(operation);
        Ok(file)
    }

    pub fn cancel(mut self) -> (FileCapability, FileRevision) {
        let FileRevisionReaderState {
            file,
            expected,
            position: _,
            operation,
        } = self
            .state
            .take()
            .expect("file revision reader retains armed state");
        drop(operation);
        (file, expected)
    }
}

impl Drop for FileRevisionReader {
    fn drop(&mut self) {
        if self.state.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "file readers must call finish to prove EOF and final binding; Drop only cancels the read"]
pub struct FileReader<'a> {
    pub(crate) file: &'a FileCapability,
    pub(crate) operation: CapabilityOperation,
    pub(crate) position: u64,
    pub(crate) max_bytes: u64,
}

impl fmt::Debug for FileReader<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FileReader").finish_non_exhaustive()
    }
}

impl FileReader<'_> {
    pub fn finish(self) -> io::Result<()> {
        let mut probe = [0_u8; 1];
        if platform::read_at(&self.file.handle, &mut probe, self.position)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file capability was not read to completion",
            ));
        }
        self.file.validate(&self.operation)
    }
}

impl Read for FileReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.position == self.max_bytes {
            let mut probe = [0_u8; 1];
            return match platform::read_at(&self.file.handle, &mut probe, self.position)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file capability exceeded its read bound",
                )),
            };
        }
        let allowed = usize::try_from((self.max_bytes - self.position).min(bytes.len() as u64))
            .map_err(|_| io::Error::other("file read bound does not fit this platform"))?;
        let read = platform::read_at(&self.file.handle, &mut bytes[..allowed], self.position)?;
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file read offset overflowed"))?;
        Ok(read)
    }
}
