use super::*;

#[must_use = "file create effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileCreateOutcome {
    Created(StagedFile),
    NoEffect(io::Error),
    AppliedUnverified(FileCreateObligation),
}

#[must_use = "file create obligations must be reconciled"]
pub struct FileCreateObligation {
    pub(crate) error: io::Error,
    pub(crate) token: StageCreateToken,
}

impl_redacted_debug!(FileCreateObligation);

impl FileCreateObligation {
    pub fn reconcile(mut self) -> FileCreateResolution {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return FileCreateResolution::Indeterminate(self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return FileCreateResolution::Indeterminate(self),
        };
        match reconcile_recovery_stage_create(&authority, &mut self.token) {
            Ok(Some((parent, name))) => {
                return match execute_stage_create(
                    &parent, &name, &authority, &operation, self.token,
                ) {
                    FileCreateOutcome::Created(staged) => FileCreateResolution::Created(staged),
                    FileCreateOutcome::AppliedUnverified(obligation) => {
                        FileCreateResolution::Indeterminate(obligation)
                    }
                    FileCreateOutcome::NoEffect(error) => FileCreateResolution::NoEffect(error),
                };
            }
            Ok(None) => {}
            Err(_) => return FileCreateResolution::Indeterminate(self),
        }
        match finish_stage_create(&authority, &operation, &mut self.token) {
            Ok(staged) => FileCreateResolution::Created(staged),
            Err(_) => FileCreateResolution::Indeterminate(self),
        }
    }
}

impl_error_accessor!(FileCreateObligation);

pub(crate) fn finish_stage_create(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    reservation: &mut StageCreateToken,
) -> io::Result<StagedFile> {
    let mut guard = authority.take_stage_create(operation, reservation)?;
    if guard.record().phase != StageCreatePhase::Applied {
        return Err(io::Error::other(
            "stage create effect is not yet classified",
        ));
    }
    guard.record().parent.validate(operation)?;
    let created = guard
        .record()
        .created
        .as_ref()
        .ok_or_else(|| io::Error::other("stage create authority is not retained"))?;
    let identity = platform::file_identity(created)?;
    if platform::file_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        identity,
    )? != platform::BindingState::Exact
    {
        return Err(identity_changed("created stage binding is not exact"));
    }
    let cleanup = platform::clone_stage_cleanup(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        created,
        identity,
    )?;
    let parent = guard.record().parent.clone();
    let name = guard.record().name.clone();
    let stage_token = authority.register_stage_record(
        parent.clone(),
        name.clone(),
        identity,
        cleanup,
        reservation,
        operation,
    )?;
    let handle = guard
        .record_mut()
        .created
        .take()
        .expect("classified stage create retains its handle");
    guard.finish_transfer(reservation);
    Ok(StagedFile {
        file: FileCapability::new(handle, identity, parent, name, Arc::downgrade(authority)),
        token: stage_token,
    })
}

pub(crate) fn execute_stage_create(
    directory: &Directory,
    name: &LeafName,
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    mut reservation: StageCreateToken,
) -> FileCreateOutcome {
    let handle = match platform::create_file(directory.inner.handle(), name.as_os_str()) {
        Ok(handle) => handle,
        Err(platform::CreateFileError::NoEffect(error)) => {
            match authority.take_stage_create(operation, &reservation) {
                Ok(guard) => {
                    if let Err(settlement) = guard.disarm(&mut reservation, operation) {
                        return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                            error: io::Error::other(format!(
                                "stage create had no native effect but its recovery record did not settle: {error}; {settlement}"
                            )),
                            token: reservation,
                        });
                    }
                }
                Err(settlement) => {
                    return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                        error: io::Error::other(format!(
                            "stage create had no native effect but its reservation did not settle: {error}; {settlement}"
                        )),
                        token: reservation,
                    });
                }
            }
            return FileCreateOutcome::NoEffect(error);
        }
        Err(platform::CreateFileError::AppliedUnverified { error, retained }) => {
            authority.attach_stage_create(&reservation, retained);
            return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                error,
                token: reservation,
            });
        }
    };
    authority.attach_stage_create(&reservation, handle);
    match finish_stage_create(authority, operation, &mut reservation) {
        Ok(staged) => FileCreateOutcome::Created(staged),
        Err(error) => FileCreateOutcome::AppliedUnverified(FileCreateObligation {
            error,
            token: reservation,
        }),
    }
}

#[must_use = "file create resolutions must be handled"]
#[derive(Debug)]
pub enum FileCreateResolution {
    Created(StagedFile),
    NoEffect(io::Error),
    Indeterminate(FileCreateObligation),
}

pub type FilePromotionOutcome =
    Outcome<FileCapability, SealedStagedFile, Box<FilePromotionObligation>>;

#[must_use = "file promotion obligations must be reconciled"]
pub struct FilePromotionObligation {
    pub(crate) error: io::Error,
    pub(crate) retained: SealedStagedFile,
    pub(crate) destination: Directory,
    pub(crate) destination_name: LeafName,
    pub(crate) attempt_id: u64,
    pub(crate) receipt: platform::PublicationReceipt,
}

impl_redacted_debug!(FilePromotionObligation);

impl FilePromotionObligation {
    pub fn reconcile(mut self) -> FilePromotionResolution {
        let authority = match self.retained.file.parent.authority() {
            Ok(authority) => authority,
            Err(_) => return FilePromotionResolution::Indeterminate(Box::new(self)),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return FilePromotionResolution::Indeterminate(Box::new(self)),
        };
        if self.retained.file.parent.validate(&operation).is_err()
            || self.destination.validate(&operation).is_err()
            || platform::file_identity(&self.retained.file.handle).ok()
                != Some(self.retained.file.identity)
            || self
                .retained
                .file
                .validate_content_revision_in(&operation, &self.retained.revision)
                .is_err()
        {
            return FilePromotionResolution::Indeterminate(Box::new(self));
        }
        let source = platform::file_binding_state(
            self.retained.file.parent.inner.handle(),
            self.retained.file.name.as_os_str(),
            self.retained.file.identity,
        );
        let destination = platform::file_binding_state(
            self.destination.inner.handle(),
            self.destination_name.as_os_str(),
            self.retained.file.identity,
        );
        match (source, destination) {
            (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
                if self
                    .retained
                    .token
                    .validate_publication_attempt(self.attempt_id, &self.receipt)
                    .is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                let settlement = platform::settle_publication(
                    &mut self.receipt,
                    self.attempt_id,
                    &self.retained.file.handle,
                    self.retained.file.parent.inner.handle(),
                    self.retained.file.name.as_os_str(),
                    self.destination.inner.handle(),
                    self.destination_name.as_os_str(),
                );
                self.retained
                    .token
                    .record_publication(self.attempt_id, self.receipt.clone());
                if let Err(error) = settlement {
                    self.error = error;
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                if self.destination.validate(&operation).is_err() {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                if let Err(error) = complete_recovery_publication(
                    &authority,
                    &operation,
                    &self.retained.token,
                    &self.retained.file,
                    &self.retained.revision,
                    &self.destination,
                    &self.destination_name,
                ) {
                    self.error = error;
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                let handle = match platform::open_file(
                    self.destination.inner.handle(),
                    self.destination_name.as_os_str(),
                ) {
                    Ok(handle)
                        if platform::file_identity(&handle).ok()
                            == Some(self.retained.file.identity) =>
                    {
                        handle
                    }
                    _ => return FilePromotionResolution::Indeterminate(Box::new(self)),
                };
                let applied = FileCapability::new(
                    handle,
                    self.retained.file.identity,
                    self.destination.clone(),
                    self.destination_name.clone(),
                    self.retained.file.authority.clone(),
                );
                if applied.validate(&operation).is_err()
                    || applied
                        .validate_content_revision_in(&operation, &self.retained.revision)
                        .is_err()
                    || self.retained.token.disarm().is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                FilePromotionResolution::Applied(applied)
            }
            (
                Ok(platform::BindingState::Exact),
                Ok(platform::BindingState::Absent | platform::BindingState::Occupied),
            ) if self.receipt.is_attempted() => {
                if self.retained.file.validate(&operation).is_err()
                    || self
                        .retained
                        .file
                        .validate_revision_in(&operation, &self.retained.revision)
                        .is_err()
                    || self
                        .retained
                        .token
                        .update(StageRegistryPhase::Sealed)
                        .is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                FilePromotionResolution::NoEffect(self.retained)
            }
            _ => FilePromotionResolution::Indeterminate(Box::new(self)),
        }
    }
}

impl_error_accessor!(FilePromotionObligation);

pub type FilePromotionResolution =
    Resolution<FileCapability, SealedStagedFile, Box<FilePromotionObligation>>;

pub enum ReplaceDestination {
    Vacant {
        parent: Directory,
        name: LeafName,
    },
    Existing(FileParkRequest),
    /// A changed destination was restored without granting deletion authority.
    /// Callers need a fresh content admission before another replacement attempt.
    Preserved(FileCapability),
}

impl_redacted_debug!(ReplaceDestination);

#[must_use = "file replacement effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileReplaceOutcome {
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        error: io::Error,
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
    AppliedUnverified(FileReplaceObligation),
}

#[must_use = "file replacement resolutions must be handled"]
#[derive(Debug)]
pub enum FileReplaceResolution {
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
    Indeterminate(FileReplaceObligation),
}

pub(crate) struct ExpectedContentReceipt {
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
}

impl ExpectedContentReceipt {
    pub(crate) fn capture(request: &FileParkRequest) -> Self {
        Self {
            authority: request.expected.revision.authority.clone(),
            identity: request.expected.revision.identity,
            size: request.expected.revision.size,
            sha256: request.expected.sha256,
        }
    }

    pub(crate) fn rebuild_after_restore(self, file: FileCapability) -> ReplaceDestination {
        use sha2::Digest;

        let revision = match file.revision() {
            Ok(revision)
                if Weak::ptr_eq(&revision.authority, &self.authority)
                    && revision.identity == self.identity
                    && revision.size == self.size =>
            {
                revision
            }
            Ok(_) | Err(_) => return ReplaceDestination::Preserved(file),
        };
        let mut reader = match file.reader(self.size) {
            Ok(reader) => reader,
            Err(_) => return ReplaceDestination::Preserved(file),
        };
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(_) => return ReplaceDestination::Preserved(file),
            }
        }
        if reader.finish().is_err()
            || file.validate_revision(&revision).is_err()
            || <[u8; 32]>::from(hasher.finalize()) != self.sha256
        {
            return ReplaceDestination::Preserved(file);
        }
        ReplaceDestination::Existing(
            file.park_request(ExpectedFileContent::new(revision, self.sha256)),
        )
    }
}

pub(crate) enum FileReplaceObligationState {
    Parking {
        park: FileParkObligation,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
    Promoting {
        promotion: Box<FilePromotionObligation>,
        displaced: Option<ParkedFile>,
        fallback: ReplaceDestination,
        receipt: Option<ExpectedContentReceipt>,
    },
    RestoreParked {
        parked: ParkedFile,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
    RestoreObligation {
        restore: FileRestoreObligation,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
}

#[must_use = "file replacement obligations must be reconciled"]
pub struct FileReplaceObligation {
    pub(crate) error: io::Error,
    pub(crate) state: Option<Box<FileReplaceObligationState>>,
}

impl_redacted_debug!(FileReplaceObligation);

impl FileReplaceObligation {
    pub fn reconcile(mut self) -> FileReplaceResolution {
        let state = *self.state.take().expect("replace obligation retains state");
        settle_file_replace(state).unwrap_or_else(|state| {
            self.state = Some(state);
            FileReplaceResolution::Indeterminate(self)
        })
    }
}

impl_error_accessor!(FileReplaceObligation);

#[must_use = "a staged file must be sealed, discarded, or retained as an obligation"]
pub struct StagedFile {
    pub(crate) file: FileCapability,
    pub(crate) token: StageToken,
}

impl_redacted_debug!(StagedFile);

impl StagedFile {
    pub fn writer(&mut self) -> io::Result<StagedWriter<'_>> {
        let authority = self.file.parent.authority()?;
        let operation = authority.enter()?;
        self.file.validate(&operation)?;
        validate_recovery_stage_writable(&authority, &operation, &self.file, &self.token)?;
        self.file.handle.set_len(0)?;
        Ok(StagedWriter {
            staged: self,
            operation,
            position: 0,
        })
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer()?;
        writer.write_all(bytes)?;
        writer.finish()
    }

    /// Makes the staged bytes durable and advances the file to sealed authority.
    pub fn seal(self) -> Result<SealedStagedFile, StageSealFailure> {
        self.seal_inner(true)
    }

    /// Seals scratch bytes whose durability is deferred until promotion.
    /// Recoverable stages journal their sealed proof and stay durable at seal.
    pub(crate) fn seal_scratch(self) -> Result<SealedStagedFile, StageSealFailure> {
        self.seal_inner(false)
    }

    pub(crate) fn seal_inner(self, durable: bool) -> Result<SealedStagedFile, StageSealFailure> {
        let authority = match self.file.parent.authority() {
            Ok(authority) => authority,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        if let Err(error) = self.file.validate(&operation) {
            return Err(StageSealFailure::new(error, self));
        }
        let (size, stamp) = match platform::file_receipt_fields(&self.file.handle) {
            Ok(receipt) => receipt,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        let revision = FileRevision {
            authority: self.file.authority.clone(),
            identity: self.file.identity,
            size,
            stamp,
        };
        let durable = match if durable {
            Ok(true)
        } else {
            stage_has_recovery(&authority, &self.token)
        } {
            Ok(durable) => durable,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        let published = if durable {
            match admit_publication_bytes(&self.file) {
                Ok(handle) => Some(handle),
                Err(error) => return Err(StageSealFailure::new(error, self)),
            }
        } else {
            None
        };
        if let Err(error) = self.file.validate_revision_in(&operation, &revision) {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) =
            seal_recovery_stage(&authority, &operation, &self.file, &revision, &self.token)
        {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.token.update(StageRegistryPhase::Sealed) {
            return Err(StageSealFailure::new(error, self));
        }
        Ok(SealedStagedFile {
            file: self.file,
            token: self.token,
            revision,
            published,
        })
    }

    pub fn discard(self) -> StageDiscardOutcome {
        let Self { file, mut token } = self;
        drop(file);
        match token.discard() {
            Ok(()) => StageDiscardOutcome::Discarded,
            Err(error) => StageDiscardOutcome::AppliedUnverified(StageDiscardObligation {
                error,
                token: Some(token),
            }),
        }
    }
}

#[must_use = "stage seal failures retain the staged file"]
pub struct StageSealFailure {
    pub(crate) error: io::Error,
    pub(crate) staged: Option<StagedFile>,
}

impl_redacted_debug!(StageSealFailure);

impl StageSealFailure {
    pub(crate) fn new(error: io::Error, staged: StagedFile) -> Self {
        Self {
            error,
            staged: Some(staged),
        }
    }

    pub fn into_staged(mut self) -> StagedFile {
        self.staged.take().expect("failed stage is retained")
    }
}

impl_error_accessor!(StageSealFailure);

#[must_use = "a sealed stage must be published, discarded, or retained as an obligation"]
pub struct SealedStagedFile {
    pub(crate) file: FileCapability,
    pub(crate) token: StageToken,
    pub(crate) revision: FileRevision,
    /// Shared read handle that becomes the promoted file's capability; absent
    /// until scratch-sealed bytes are made durable for promotion.
    pub(crate) published: Option<File>,
}

impl_redacted_debug!(SealedStagedFile);

impl SealedStagedFile {
    pub fn discard(self) -> StageDiscardOutcome {
        let Self {
            file,
            token,
            revision: _,
            published,
        } = self;
        drop(published);
        StagedFile { file, token }.discard()
    }
}

#[must_use = "stage discard effects must be explicitly settled"]
#[derive(Debug)]
pub enum StageDiscardOutcome {
    Discarded,
    AppliedUnverified(StageDiscardObligation),
}

#[must_use = "stage discard resolutions must be handled"]
#[derive(Debug)]
pub enum StageDiscardResolution {
    Discarded,
    Indeterminate(StageDiscardObligation),
}

#[must_use = "stage discard obligations must be reconciled"]
pub struct StageDiscardObligation {
    pub(crate) error: io::Error,
    pub(crate) token: Option<StageToken>,
}

impl_redacted_debug!(StageDiscardObligation);

impl StageDiscardObligation {
    pub fn reconcile(mut self) -> StageDiscardResolution {
        let mut token = self.token.take().expect("discard obligation retains token");
        match token.discard() {
            Ok(()) => StageDiscardResolution::Discarded,
            Err(_) => {
                self.token = Some(token);
                StageDiscardResolution::Indeterminate(self)
            }
        }
    }
}

impl_error_accessor!(StageDiscardObligation);

impl SealedStagedFile {
    pub fn replace_nondurable(self, destination: ReplaceDestination) -> FileReplaceOutcome {
        match destination {
            ReplaceDestination::Vacant { parent, name } => {
                let fallback = ReplaceDestination::Vacant {
                    parent: parent.clone(),
                    name: name.clone(),
                };
                continue_file_replace(self, &parent, &name, None, fallback, None)
            }
            ReplaceDestination::Existing(request) => {
                let receipt = ExpectedContentReceipt::capture(&request);
                let parent = request.file.parent.clone();
                let name = request.file.name.clone();
                match parent.park_file(request) {
                    FileParkOutcome::Parked(displaced) => continue_file_replace(
                        self,
                        &parent,
                        &name,
                        Some(displaced),
                        ReplaceDestination::Vacant {
                            parent: parent.clone(),
                            name: name.clone(),
                        },
                        Some(receipt),
                    ),
                    FileParkOutcome::NoEffect { error, request } => FileReplaceOutcome::NoEffect {
                        error,
                        staged: self,
                        destination: ReplaceDestination::Existing(request),
                    },
                    FileParkOutcome::Preserved { error, file } => FileReplaceOutcome::NoEffect {
                        error,
                        staged: self,
                        destination: ReplaceDestination::Preserved(file),
                    },
                    FileParkOutcome::AppliedUnverified(park) => {
                        FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                            error: io::Error::other(
                                "replacement destination park is not yet settled",
                            ),
                            state: Some(Box::new(FileReplaceObligationState::Parking {
                                park,
                                staged: self,
                                receipt,
                            })),
                        })
                    }
                }
            }
            ReplaceDestination::Preserved(file) => FileReplaceOutcome::NoEffect {
                error: identity_changed(
                    "preserved replacement destination requires fresh content admission",
                ),
                staged: self,
                destination: ReplaceDestination::Preserved(file),
            },
        }
    }

    pub fn promote_no_replace(
        self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
    ) -> FilePromotionOutcome {
        self.promote_no_replace_internal(
            source_parent,
            destination_parent,
            destination_name,
            None,
            true,
        )
    }

    /// Like `promote_no_replace`, but the caller must `Directory::sync` both parents before
    /// relying on the publication being durable.
    pub fn promote_no_replace_deferring_parent_sync(
        self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
    ) -> FilePromotionOutcome {
        self.promote_no_replace_internal(
            source_parent,
            destination_parent,
            destination_name,
            None,
            false,
        )
    }

    pub(crate) fn promote_no_replace_internal(
        mut self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
        displaced: Option<&ParkedFile>,
        sync_parents: bool,
    ) -> FilePromotionOutcome {
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FilePromotionOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return FilePromotionOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.file.validate_bound_to(source_parent, &operation) {
            return FilePromotionOutcome::NoEffect { error, item: self };
        }
        if self.published.is_none() {
            match admit_publication_bytes(&self.file) {
                Ok(handle) => self.published = Some(handle),
                Err(error) => {
                    return FilePromotionOutcome::NoEffect { error, item: self };
                }
            }
        }
        if let Err(error) = self.file.validate_revision_in(&operation, &self.revision) {
            return FilePromotionOutcome::NoEffect { error, item: self };
        }
        if let Err(error) = destination_parent.validate(&operation) {
            return FilePromotionOutcome::NoEffect { error, item: self };
        }
        if !Weak::ptr_eq(
            &source_parent.inner.authority,
            &destination_parent.inner.authority,
        ) {
            return FilePromotionOutcome::NoEffect {
                error: stale_capability(),
                item: self,
            };
        }
        let expected_recovery = match validate_recovery_publication(
            &authority,
            &operation,
            &self.token,
            &self.file,
            &self.revision,
            source_parent,
            destination_parent,
            destination_name,
            displaced,
        ) {
            Ok(expected) => expected,
            Err(error) => {
                return FilePromotionOutcome::NoEffect { error, item: self };
            }
        };
        let attempt_id = match self.token.allocate_publication_attempt() {
            Ok(attempt_id) => attempt_id,
            Err(error) => {
                return FilePromotionOutcome::NoEffect { error, item: self };
            }
        };
        let mut attempt = match platform::prepare_publication(
            attempt_id,
            &self.file.handle,
            self.revision.size,
            self.revision.stamp,
            source_parent.inner.handle(),
            self.file.name.as_os_str(),
            destination_parent.inner.handle(),
            destination_name.as_os_str(),
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                return FilePromotionOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.token.prepare_promotion(
            destination_parent,
            destination_name,
            attempt_id,
            attempt.clone(),
            displaced.map(|parked| &parked.token),
            expected_recovery.as_ref(),
        ) {
            return FilePromotionOutcome::NoEffect { error, item: self };
        }

        let rename = platform::rename_no_replace(
            &mut attempt,
            attempt_id,
            source_parent.inner.handle(),
            self.file.name.as_os_str(),
            &self.file.handle,
            destination_parent.inner.handle(),
            destination_name.as_os_str(),
        );
        let (mut rename_error, mut receipt) = match rename {
            Ok(()) => {
                self.token.record_publication(attempt_id, attempt.clone());
                (None, attempt)
            }
            Err(error) => (Some(error), attempt),
        };
        let source = platform::file_binding_state(
            source_parent.inner.handle(),
            self.file.name.as_os_str(),
            self.file.identity,
        );
        let destination = platform::file_binding_state(
            destination_parent.inner.handle(),
            destination_name.as_os_str(),
            self.file.identity,
        );

        match (source, destination) {
            (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
                if let Err(error) = self
                    .token
                    .validate_publication_attempt(attempt_id, &receipt)
                {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
                }
                let settle = if sync_parents {
                    platform::settle_publication
                } else {
                    platform::settle_publication_deferring_parent_sync
                };
                let settlement = settle(
                    &mut receipt,
                    attempt_id,
                    &self.file.handle,
                    source_parent.inner.handle(),
                    self.file.name.as_os_str(),
                    destination_parent.inner.handle(),
                    destination_name.as_os_str(),
                );
                self.token.record_publication(attempt_id, receipt.clone());
                if let Err(error) = settlement {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
                }
                if let Err(error) = complete_recovery_publication(
                    &authority,
                    &operation,
                    &self.token,
                    &self.file,
                    &self.revision,
                    destination_parent,
                    destination_name,
                ) {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
                }
                match self
                    .published
                    .as_ref()
                    .ok_or_else(|| identity_changed("promoted file lost its read capability"))
                    .and_then(File::try_clone)
                {
                    Ok(handle)
                        if platform::file_identity(&handle).ok() == Some(self.file.identity) =>
                    {
                        let applied = FileCapability::new(
                            handle,
                            self.file.identity,
                            destination_parent.clone(),
                            destination_name.clone(),
                            self.file.authority.clone(),
                        );
                        if applied.validate(&operation).is_err()
                            || applied
                                .validate_content_revision_in(&operation, &self.revision)
                                .is_err()
                            || self.token.disarm().is_err()
                        {
                            FilePromotionOutcome::AppliedUnverified(Box::new(
                                FilePromotionObligation {
                                    error: identity_changed(
                                        "promoted file lost its authority chain",
                                    ),
                                    retained: self,
                                    destination: destination_parent.clone(),
                                    destination_name: destination_name.clone(),
                                    attempt_id,
                                    receipt,
                                },
                            ))
                        } else {
                            FilePromotionOutcome::Applied(applied)
                        }
                    }
                    Ok(_) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error: identity_changed(
                                "promoted file changed before read capability admission",
                            ),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                    Err(error) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                }
            }
            (
                Ok(platform::BindingState::Exact),
                Ok(platform::BindingState::Absent | platform::BindingState::Occupied),
            ) if receipt.is_attempted() => {
                let error = rename_error.take().unwrap_or_else(|| {
                    identity_changed("promotion reported success without changing topology")
                });
                match self.token.update(StageRegistryPhase::Sealed) {
                    Ok(()) => FilePromotionOutcome::NoEffect { error, item: self },
                    Err(update) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error: io::Error::other(format!(
                                "promotion failed and stage registry could not settle: {error}; {update}"
                            )),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                }
            }
            _ => FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                error: rename_error.take().unwrap_or_else(|| {
                    identity_changed("file promotion effect could not be verified")
                }),
                retained: self,
                destination: destination_parent.clone(),
                destination_name: destination_name.clone(),
                attempt_id,
                receipt,
            })),
        }
    }
}

pub(crate) fn continue_file_replace(
    staged: SealedStagedFile,
    parent: &Directory,
    name: &LeafName,
    displaced: Option<ParkedFile>,
    fallback: ReplaceDestination,
    receipt: Option<ExpectedContentReceipt>,
) -> FileReplaceOutcome {
    let source = staged.file.parent.clone();
    match staged.promote_no_replace_internal(&source, parent, name, displaced.as_ref(), true) {
        FilePromotionOutcome::Applied(current) => {
            FileReplaceOutcome::Replaced { current, displaced }
        }
        FilePromotionOutcome::NoEffect {
            error,
            item: staged,
        } => {
            let Some(displaced) = displaced else {
                return FileReplaceOutcome::NoEffect {
                    error,
                    staged,
                    destination: fallback,
                };
            };
            restore_displaced_after_failed_replace(
                staged,
                displaced,
                receipt.expect("existing replacement retains its receipt"),
                error,
            )
        }
        FilePromotionOutcome::AppliedUnverified(promotion) => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error: io::Error::other("replacement promotion is not yet settled"),
                state: Some(Box::new(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                })),
            })
        }
    }
}

pub(crate) fn restore_displaced_after_failed_replace(
    staged: SealedStagedFile,
    displaced: ParkedFile,
    receipt: ExpectedContentReceipt,
    promotion_error: io::Error,
) -> FileReplaceOutcome {
    match displaced.restore() {
        FileRestoreOutcome::Applied(file) => FileReplaceOutcome::NoEffect {
            error: promotion_error,
            staged,
            destination: receipt.rebuild_after_restore(file),
        },
        FileRestoreOutcome::NoEffect {
            error,
            item: parked,
        } => FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
            error,
            state: Some(Box::new(FileReplaceObligationState::RestoreParked {
                parked,
                staged,
                receipt,
            })),
        }),
        FileRestoreOutcome::AppliedUnverified(restore) => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error: io::Error::other("replacement rollback is not yet settled"),
                state: Some(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                })),
            })
        }
    }
}

pub(crate) fn settle_file_replace(
    state: FileReplaceObligationState,
) -> Result<FileReplaceResolution, Box<FileReplaceObligationState>> {
    match state {
        FileReplaceObligationState::Parking {
            park,
            staged,
            receipt,
        } => {
            let (parent, name) = {
                let request = park
                    .request
                    .as_ref()
                    .expect("park obligation retains request");
                (request.file.parent.clone(), request.file.name.clone())
            };
            match park.reconcile() {
                FileParkResolution::Parked(displaced) => {
                    replace_outcome_to_resolution(continue_file_replace(
                        staged,
                        &parent,
                        &name,
                        Some(displaced),
                        ReplaceDestination::Vacant {
                            parent: parent.clone(),
                            name: name.clone(),
                        },
                        Some(receipt),
                    ))
                }
                FileParkResolution::NoEffect(request) => Ok(FileReplaceResolution::NoEffect {
                    staged,
                    destination: ReplaceDestination::Existing(request),
                }),
                FileParkResolution::Preserved { file, .. } => Ok(FileReplaceResolution::NoEffect {
                    staged,
                    destination: ReplaceDestination::Preserved(file),
                }),
                FileParkResolution::Indeterminate(park) => {
                    Err(Box::new(FileReplaceObligationState::Parking {
                        park,
                        staged,
                        receipt,
                    }))
                }
            }
        }
        FileReplaceObligationState::Promoting {
            promotion,
            displaced,
            fallback,
            receipt,
        } => match (*promotion).reconcile() {
            FilePromotionResolution::Applied(current) => {
                Ok(FileReplaceResolution::Replaced { current, displaced })
            }
            FilePromotionResolution::NoEffect(staged) => {
                let Some(displaced) = displaced else {
                    return Ok(FileReplaceResolution::NoEffect {
                        staged,
                        destination: fallback,
                    });
                };
                replace_outcome_to_resolution(restore_displaced_after_failed_replace(
                    staged,
                    displaced,
                    receipt.expect("existing replacement retains its receipt"),
                    io::Error::other("replacement promotion had no effect"),
                ))
            }
            FilePromotionResolution::Indeterminate(promotion) => {
                Err(Box::new(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                }))
            }
        },
        FileReplaceObligationState::RestoreParked {
            parked,
            staged,
            receipt,
        } => match parked.restore() {
            FileRestoreOutcome::Applied(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: receipt.rebuild_after_restore(file),
            }),
            FileRestoreOutcome::NoEffect { item: parked, .. } => {
                Err(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                }))
            }
            FileRestoreOutcome::AppliedUnverified(restore) => {
                Err(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                }))
            }
        },
        FileReplaceObligationState::RestoreObligation {
            restore,
            staged,
            receipt,
        } => match restore.reconcile() {
            FileRestoreResolution::Applied(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: receipt.rebuild_after_restore(file),
            }),
            FileRestoreResolution::NoEffect(parked) => {
                Err(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                }))
            }
            FileRestoreResolution::Indeterminate(restore) => {
                Err(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                }))
            }
        },
    }
}

pub(crate) fn replace_outcome_to_resolution(
    outcome: FileReplaceOutcome,
) -> Result<FileReplaceResolution, Box<FileReplaceObligationState>> {
    match outcome {
        FileReplaceOutcome::Replaced { current, displaced } => {
            Ok(FileReplaceResolution::Replaced { current, displaced })
        }
        FileReplaceOutcome::NoEffect {
            staged,
            destination,
            ..
        } => Ok(FileReplaceResolution::NoEffect {
            staged,
            destination,
        }),
        FileReplaceOutcome::AppliedUnverified(mut obligation) => Err(obligation
            .state
            .take()
            .expect("replace obligation retains state")),
    }
}

pub struct StagedWriter<'a> {
    pub(crate) staged: &'a mut StagedFile,
    pub(crate) operation: CapabilityOperation,
    pub(crate) position: u64,
}

impl fmt::Debug for StagedWriter<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedWriter")
            .finish_non_exhaustive()
    }
}

impl StagedWriter<'_> {
    /// Completes this writer; sealing the stage is the durability boundary.
    pub fn finish(self) -> io::Result<()> {
        self.staged.file.validate(&self.operation)
    }
}

impl Write for StagedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = platform::write_at(&self.staged.file.handle, bytes, self.position)?;
        self.position = self
            .position
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("staged file write offset overflowed"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.staged.file.handle.sync_data()
    }
}
