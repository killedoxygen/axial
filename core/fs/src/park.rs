use super::*;

#[must_use = "file park effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileParkOutcome {
    Parked(ParkedFile),
    NoEffect {
        error: io::Error,
        request: FileParkRequest,
    },
    /// The original binding was restored with stable but different content.
    Preserved {
        error: io::Error,
        file: FileCapability,
    },
    AppliedUnverified(FileParkObligation),
}

#[must_use = "file park resolutions must be handled"]
#[derive(Debug)]
pub enum FileParkResolution {
    Parked(ParkedFile),
    NoEffect(FileParkRequest),
    /// The original binding was restored with stable but different content.
    Preserved {
        error: io::Error,
        file: FileCapability,
    },
    Indeterminate(FileParkObligation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileParkPhase {
    Parking,
    RestoringRejectedReceipt,
}

pub(crate) enum RestoredFileProof {
    Current(FileRevision),
    Preserved,
}

#[cfg(test)]
pub(crate) struct RestoredFileProofPause {
    pub(crate) hashed: Arc<std::sync::Barrier>,
    pub(crate) resume: Arc<std::sync::Barrier>,
}

#[must_use = "file park obligations must be reconciled"]
pub struct FileParkObligation {
    pub(crate) error: io::Error,
    pub(crate) request: Option<FileParkRequest>,
    pub(crate) token: FileParkRegistryToken,
    pub(crate) park_name: LeafName,
    pub(crate) phase: FileParkPhase,
    pub(crate) digest_verified: bool,
    #[cfg(test)]
    pub(crate) restored_proof_pause: Option<RestoredFileProofPause>,
}

impl_redacted_debug!(FileParkObligation);

impl FileParkObligation {
    pub fn reconcile(self) -> FileParkResolution {
        settle_file_park(self, false)
    }

    pub fn restore(self) -> FileParkResolution {
        settle_file_park(self, true)
    }
}

impl_error_accessor!(FileParkObligation);

#[must_use = "a parked file must be removed, restored, acknowledged as preserved, or retained as an obligation"]
pub struct ParkedFile {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) park_name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) stamp: platform::FileStamp,
    pub(crate) verified: bool,
    pub(crate) token: FileParkRegistryToken,
    pub(crate) authority: Weak<CapabilityAuthority>,
}

impl_redacted_debug!(ParkedFile);

#[must_use = "failed preservation acknowledgement retains the parked file authority"]
pub struct FileParkPreservationError {
    pub(crate) error: io::Error,
    pub(crate) parked: ParkedFile,
}

impl_redacted_debug!(FileParkPreservationError);

impl FileParkPreservationError {
    pub fn into_parked(self) -> ParkedFile {
        self.parked
    }
}

impl_error_accessor!(FileParkPreservationError);

pub type FileRemovalOutcome = Outcome<(), ParkedFile, FileRemovalObligation>;

pub type FileRemovalResolution = Resolution<(), ParkedFile, FileRemovalObligation>;

#[must_use = "file removal obligations must be reconciled"]
pub struct FileRemovalObligation {
    pub(crate) error: io::Error,
    pub(crate) parked: Option<ParkedFile>,
}

impl_redacted_debug!(FileRemovalObligation);

impl FileRemovalObligation {
    pub fn reconcile(mut self) -> FileRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_admitted(
        mut self,
        mut parked: ParkedFile,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            drop(operation);
            return self.retain(parked);
        }
        let guard = match authority.take_file_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_removed_file(
            guard.record().parent.inner.handle(),
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        ) {
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                FileRemovalResolution::Applied(())
            }
            Ok(()) => {
                drop(operation);
                self.parked = Some(parked);
                FileRemovalResolution::Indeterminate(self)
            }
            Err(_) => {
                drop(operation);
                self.parked = Some(parked);
                FileRemovalResolution::Indeterminate(self)
            }
        }
    }

    pub(crate) fn retain(mut self, parked: ParkedFile) -> FileRemovalResolution {
        self.parked = Some(parked);
        FileRemovalResolution::Indeterminate(self)
    }
}

impl_error_accessor!(FileRemovalObligation);

pub type FileRestoreOutcome = Outcome<FileCapability, ParkedFile, FileRestoreObligation>;

pub type FileRestoreResolution = Resolution<FileCapability, ParkedFile, FileRestoreObligation>;

#[must_use = "file restore obligations must be reconciled"]
pub struct FileRestoreObligation {
    pub(crate) error: io::Error,
    pub(crate) parked: Option<ParkedFile>,
}

impl_redacted_debug!(FileRestoreObligation);

impl FileRestoreObligation {
    pub fn reconcile(mut self) -> FileRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_admitted(
        self,
        parked: ParkedFile,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRestoreResolution {
        match settle_file_restore_admitted(parked, authority, operation) {
            FileRestoreSettlement::Restored(file) => FileRestoreResolution::Applied(file),
            FileRestoreSettlement::NoEffect(parked) => FileRestoreResolution::NoEffect(parked),
            FileRestoreSettlement::Indeterminate(parked) => self.retain(parked),
        }
    }

    pub(crate) fn retain(mut self, parked: ParkedFile) -> FileRestoreResolution {
        self.parked = Some(parked);
        FileRestoreResolution::Indeterminate(self)
    }
}

impl_error_accessor!(FileRestoreObligation);

pub(crate) enum FileRestoreSettlement {
    Restored(FileCapability),
    NoEffect(ParkedFile),
    Indeterminate(ParkedFile),
}

impl ParkedFile {
    pub fn validate_current(&self) -> io::Result<()> {
        let (operation, guard) = self.checkout_current()?;
        drop(guard);
        drop(operation);
        Ok(())
    }

    pub fn acknowledge_preserved(mut self) -> Result<(), FileParkPreservationError> {
        let (operation, guard) = match self.checkout_current() {
            Ok(current) => current,
            Err(error) => {
                return Err(FileParkPreservationError {
                    error,
                    parked: self,
                });
            }
        };
        guard.disarm(&mut self.token, &operation);
        Ok(())
    }

    pub(crate) fn checkout_current(
        &self,
    ) -> io::Result<(CapabilityOperation, FileParkRecordGuard)> {
        if !self.verified {
            return Err(identity_changed(
                "unverified parked file has no exact receipt",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter_file_park(&self.token)?;
        let guard = authority.take_file_park(&operation, &self.token)?;
        if let Err(error) = self.validate_checked_out(&operation, guard.record()) {
            drop(guard);
            return Err(error);
        }
        Ok((operation, guard))
    }

    pub fn remove(mut self) -> FileRemovalOutcome {
        if !self.verified {
            return FileRemovalOutcome::NoEffect {
                error: identity_changed("unverified parked file can only be restored"),
                item: self,
            };
        }
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return FileRemovalOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_file(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                FileRemovalOutcome::Applied(())
            }
            Ok(()) => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                error: identity_changed("file removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        FileRemovalOutcome::NoEffect { error, item: self }
                    }
                    _ => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn remove_with_recovery(self, permit: &DrainRecoveryPermit) -> FileRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        self.remove_admitted(authority, operation)
    }

    pub(crate) fn remove_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRemovalOutcome {
        if !self.verified {
            return FileRemovalOutcome::NoEffect {
                error: identity_changed("unverified parked file can only be restored"),
                item: self,
            };
        }
        if let Err(error) = self.validate(&operation) {
            return FileRemovalOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_file(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                FileRemovalOutcome::Applied(())
            }
            Ok(()) => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                error: identity_changed("file removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        FileRemovalOutcome::NoEffect { error, item: self }
                    }
                    _ => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub fn restore(mut self) -> FileRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_file(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = FileCapability::new(
                    handle,
                    self.identity,
                    self.parent.clone(),
                    self.original_name.clone(),
                    self.authority.clone(),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    FileRestoreOutcome::Applied(restored)
                } else {
                    FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error: identity_changed("restored file lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        FileRestoreOutcome::NoEffect { error, item: self }
                    }
                    _ => FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn restore_with_recovery(self, permit: &DrainRecoveryPermit) -> FileRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        self.restore_admitted(authority, operation)
    }

    pub(crate) fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_file(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = FileCapability::new(
                    handle,
                    self.identity,
                    self.parent.clone(),
                    self.original_name.clone(),
                    self.authority.clone(),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    FileRestoreOutcome::Applied(restored)
                } else {
                    FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error: identity_changed("restored file lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        FileRestoreOutcome::NoEffect { error, item: self }
                    }
                    _ => FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.authority.upgrade().ok_or_else(stale_capability)
    }

    pub(crate) fn binding_state(&self) -> io::Result<platform::BindingState> {
        platform::file_binding_state(
            self.parent.inner.handle(),
            self.park_name.as_os_str(),
            self.identity,
        )
    }

    pub(crate) fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        {
            return Err(identity_changed("parked file capability changed"));
        }
        let registered = {
            let state = operation.authority.state()?;
            state.file_parks.get(&self.token.id).is_some_and(|record| {
                record.phase == FileParkRegistryPhase::Live
                    && record.identity == self.identity
                    && record.size == self.size
                    && record.stamp == self.stamp
                    && record.name == self.park_name
                    && record.original_name == self.original_name
            })
        };
        if !registered || self.binding_state()? != platform::BindingState::Exact {
            return Err(identity_changed("parked file capability changed"));
        }
        Ok(())
    }

    pub(crate) fn validate_checked_out(
        &self,
        operation: &CapabilityOperation,
        record: &FileParkSettlementRecord,
    ) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || record.phase != FileParkRegistryPhase::Live
            || record.identity != self.identity
            || record.size != self.size
            || record.stamp != self.stamp
            || record.name != self.park_name
            || record.original_name != self.original_name
            || platform::parked_file_receipt_fields(&record.cleanup)? != (self.size, self.stamp)
            || platform::file_binding_state(
                self.parent.inner.handle(),
                self.park_name.as_os_str(),
                self.identity,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed("parked file capability changed"));
        }
        self.parent.validate(operation)
    }
}

pub(crate) fn settle_file_restore_admitted(
    mut parked: ParkedFile,
    authority: Arc<CapabilityAuthority>,
    operation: CapabilityOperation,
) -> FileRestoreSettlement {
    if parked.parent.validate(&operation).is_err() {
        return FileRestoreSettlement::Indeterminate(parked);
    }
    let guard = match authority.take_file_park(&operation, &parked.token) {
        Ok(guard) => guard,
        Err(_) => return FileRestoreSettlement::Indeterminate(parked),
    };
    match platform::settle_restored_file(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        &guard.record().cleanup,
        guard.record().identity,
        guard.record().original_name.as_os_str(),
    ) {
        Ok(handle) => {
            let restored = FileCapability::new(
                handle,
                parked.identity,
                parked.parent.clone(),
                parked.original_name.clone(),
                parked.authority.clone(),
            );
            if restored.validate(&operation).is_ok() {
                guard.disarm(&mut parked.token, &operation);
                FileRestoreSettlement::Restored(restored)
            } else {
                drop(guard);
                FileRestoreSettlement::Indeterminate(parked)
            }
        }
        Err(_) => {
            drop(guard);
            match parked.binding_state() {
                Ok(platform::BindingState::Exact) if parked.validate(&operation).is_ok() => {
                    FileRestoreSettlement::NoEffect(parked)
                }
                _ => FileRestoreSettlement::Indeterminate(parked),
            }
        }
    }
}

pub type DirectoryParkOutcome = Outcome<ParkedDirectory, Directory, DirectoryParkObligation>;

pub type DirectoryParkResolution = Resolution<ParkedDirectory, Directory, DirectoryParkObligation>;

#[must_use = "directory park obligations must be reconciled"]
pub struct DirectoryParkObligation {
    pub(crate) error: io::Error,
    pub(crate) parent: Directory,
    pub(crate) directory: Option<Directory>,
    pub(crate) original_name: LeafName,
    pub(crate) token: DirectoryParkRegistryToken,
    pub(crate) park_name: LeafName,
}

impl_redacted_debug!(DirectoryParkObligation);

impl DirectoryParkObligation {
    pub fn reconcile(self) -> DirectoryParkResolution {
        settle_directory_park(self, false)
    }

    pub fn restore(self) -> DirectoryParkResolution {
        settle_directory_park(self, true)
    }
}

impl_error_accessor!(DirectoryParkObligation);

#[must_use = "a parked directory must be removed, restored, or retained as an obligation"]
pub struct ParkedDirectory {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) park_name: LeafName,
    pub(crate) identity: DirectoryIdentity,
    pub(crate) token: DirectoryParkRegistryToken,
    pub(crate) authority: Weak<CapabilityAuthority>,
}

impl_redacted_debug!(ParkedDirectory);

#[must_use = "a retained directory tree removal must be retried or transferred to an effect owner"]
pub struct RetainedDirectoryTreeRemoval {
    pub(crate) parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(RetainedDirectoryTreeRemoval);

impl RetainedDirectoryTreeRemoval {
    pub(crate) fn new(parked: ParkedDirectory) -> Self {
        Self {
            parked: Some(parked),
        }
    }

    pub fn retry(mut self) -> DirectoryTreeRemovalOutcome {
        self.parked
            .take()
            .expect("retained tree removal owns its parked directory")
            .remove_tree()
    }
}

impl Drop for RetainedDirectoryTreeRemoval {
    fn drop(&mut self) {
        if self.parked.is_some() {
            std::process::abort();
        }
    }
}

pub type DirectoryRemovalOutcome = Outcome<(), ParkedDirectory, DirectoryRemovalObligation>;

pub type DirectoryRemovalResolution = Resolution<(), ParkedDirectory, DirectoryRemovalObligation>;

#[must_use = "directory removal obligations must be reconciled"]
pub struct DirectoryRemovalObligation {
    pub(crate) error: io::Error,
    pub(crate) parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryRemovalObligation);

#[must_use = "directory tree removal effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryTreeRemovalOutcome {
    Removed,
    /// Native traversal did not start; the exact parked root remains owned by
    /// a retry-only carrier.
    Retained {
        error: io::Error,
        retained: RetainedDirectoryTreeRemoval,
    },
    /// The parked root topology could not be proven after the attempt.
    Indeterminate(DirectoryTreeRemovalObligation),
}

#[must_use = "directory tree removal resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryTreeRemovalResolution {
    Removed,
    Indeterminate(DirectoryTreeRemovalObligation),
}

#[must_use = "directory tree removal obligations must be reconciled"]
pub struct DirectoryTreeRemovalObligation {
    pub(crate) error: io::Error,
    pub(crate) parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryTreeRemovalObligation);

impl Drop for DirectoryTreeRemovalObligation {
    fn drop(&mut self) {
        if self.parked.is_some() {
            std::process::abort();
        }
    }
}

pub type DirectoryRestoreOutcome = Outcome<Directory, ParkedDirectory, DirectoryRestoreObligation>;

pub type DirectoryRestoreResolution =
    Resolution<Directory, ParkedDirectory, DirectoryRestoreObligation>;

#[must_use = "directory restore obligations must be reconciled"]
pub struct DirectoryRestoreObligation {
    pub(crate) error: io::Error,
    pub(crate) parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryRestoreObligation);

pub(crate) fn settle_directory_park(
    mut obligation: DirectoryParkObligation,
    force_restore: bool,
) -> DirectoryParkResolution {
    let directory_identity = obligation
        .directory
        .as_ref()
        .expect("directory park obligation retains directory")
        .inner
        .identity;
    let authority = match obligation.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    let operation = match authority.enter_directory_rebinding() {
        Ok(operation) => operation,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    if obligation.parent.validate(&operation).is_err() {
        return DirectoryParkResolution::Indeterminate(obligation);
    }
    let mut guard = match authority.take_directory_park(&operation, &obligation.token) {
        Ok(guard) => guard,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    let original = platform::directory_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().original_name.as_os_str(),
        guard.record().identity,
    );
    let parked_state = platform::directory_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        guard.record().identity,
    );
    match (original, parked_state) {
        (Ok(platform::BindingState::Exact), Ok(platform::BindingState::Absent)) => {
            guard.disarm(&mut obligation.token, &operation);
            DirectoryParkResolution::NoEffect(
                obligation.directory.take().expect("parked directory"),
            )
        }
        (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
            if force_restore {
                let restoration = {
                    let record = guard.record_mut();
                    platform::restore_parked_directory(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        &mut record.cleanup,
                        record.identity,
                        record.original_name.as_os_str(),
                    )
                };
                return match restoration {
                    Ok(_)
                        if obligation
                            .directory
                            .as_ref()
                            .expect("directory park obligation retains directory")
                            .validate(&operation)
                            .is_ok() =>
                    {
                        guard.disarm(&mut obligation.token, &operation);
                        DirectoryParkResolution::NoEffect(
                            obligation.directory.take().expect("parked directory"),
                        )
                    }
                    _ => DirectoryParkResolution::Indeterminate(obligation),
                };
            }
            let directory = obligation.directory.take().expect("parked directory");
            if obligation.parent.validate(&operation).is_err() {
                return DirectoryParkResolution::Indeterminate(obligation);
            }
            guard.record_mut().phase = DirectoryParkRegistryPhase::Live;
            drop(guard);
            DirectoryParkResolution::Applied(ParkedDirectory {
                parent: obligation.parent,
                original_name: obligation.original_name,
                park_name: obligation.park_name,
                identity: directory_identity,
                token: obligation.token,
                authority: directory.inner.authority.clone(),
            })
        }
        _ => DirectoryParkResolution::Indeterminate(obligation),
    }
}

impl ParkedDirectory {
    pub fn remove_empty(mut self) -> DirectoryRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryRemovalOutcome::Applied(())
            }
            Ok(()) => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                error: identity_changed("directory removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        DirectoryRemovalOutcome::NoEffect { error, item: self }
                    }
                    _ => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn remove_empty_with_recovery(
        self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        self.remove_empty_admitted(authority, operation)
    }

    pub(crate) fn remove_empty_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRemovalOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect { error, item: self };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryRemovalOutcome::Applied(())
            }
            Ok(()) => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                error: identity_changed("directory removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        DirectoryRemovalOutcome::NoEffect { error, item: self }
                    }
                    _ => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    /// Removes every descendant bound inside the claimed parked root without
    /// following links or reparse points. Entries concurrently introduced
    /// inside that deletion root are part of the deletion scope; its original
    /// and parked sibling bindings remain outside that scope.
    ///
    /// The capability authority and root lease serialize cooperating namespace
    /// writers. A non-cooperating process that concurrently rewrites the same
    /// private namespace is outside that authority; Linux has no unprivileged
    /// handle-targeted unlink that could close its final name/unlink race.
    pub fn remove_tree(self) -> DirectoryTreeRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        self.remove_tree_admitted(authority, operation)
    }

    pub(crate) fn remove_tree_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryTreeRemovalOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                error,
                parked: Some(self),
            });
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory_tree(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryTreeRemovalOutcome::Removed
            }
            Ok(()) => DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                error: identity_changed("directory tree removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                    error,
                    parked: Some(self),
                })
            }
        }
    }

    pub fn restore(mut self) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_directory(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    self.identity,
                    self.authority.clone(),
                    Some(DirectoryParent {
                        directory: self.parent.clone(),
                        name: self.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    DirectoryRestoreOutcome::Applied(restored)
                } else {
                    DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error: identity_changed("restored directory lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        DirectoryRestoreOutcome::NoEffect { error, item: self }
                    }
                    _ => DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn restore_with_recovery(
        self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        self.restore_admitted(authority, operation)
    }

    pub(crate) fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect { error, item: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect { error, item: self };
            }
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_directory(
                record.parent.inner.handle(),
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    self.identity,
                    self.authority.clone(),
                    Some(DirectoryParent {
                        directory: self.parent.clone(),
                        name: self.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    DirectoryRestoreOutcome::Applied(restored)
                } else {
                    DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error: identity_changed("restored directory lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                    Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                        DirectoryRestoreOutcome::NoEffect { error, item: self }
                    }
                    _ => DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    pub(crate) fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.authority.upgrade().ok_or_else(stale_capability)
    }

    pub(crate) fn binding_state(&self) -> io::Result<platform::BindingState> {
        platform::directory_binding_state(
            self.parent.inner.handle(),
            self.park_name.as_os_str(),
            self.identity.physical,
        )
    }

    pub(crate) fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        {
            return Err(identity_changed("parked directory capability changed"));
        }
        let registered = {
            let state = operation.authority.state()?;
            state
                .directory_parks
                .get(&self.token.id)
                .is_some_and(|record| {
                    record.phase == DirectoryParkRegistryPhase::Live
                        && record.identity == self.identity.physical
                        && record.name == self.park_name
                        && record.original_name == self.original_name
                })
        };
        if !registered || self.binding_state()? != platform::BindingState::Exact {
            return Err(identity_changed("parked directory capability changed"));
        }
        Ok(())
    }
}

pub(crate) fn hash_parked_file(
    file: &platform::FileCleanupHandle,
    size: u64,
) -> io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    let mut offset = 0_u64;
    while offset < size {
        let mut chunk = [0_u8; 64 * 1024];
        let wanted = usize::try_from((size - offset).min(chunk.len() as u64))
            .map_err(|_| io::Error::other("file hash bound does not fit this platform"))?;
        let read = platform::read_parked_at(file, &mut chunk[..wanted], offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file ended before its authenticated size",
            ));
        }
        digest.update(&chunk[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file hash offset overflowed"))?;
    }
    let mut probe = [0_u8; 1];
    if platform::read_parked_at(file, &mut probe, size)? != 0 {
        return Err(identity_changed(
            "file exceeded its authenticated size during hashing",
        ));
    }
    Ok(digest.finalize().into())
}

pub(crate) fn verify_parked_file(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    expected_content: &ExpectedFileContent,
) -> io::Result<(u64, platform::FileStamp)> {
    let receipt = observe_parked_revision(
        parent,
        park_name,
        parked,
        expected_content.revision.identity,
    )?;
    if receipt.0 != expected_content.revision.size
        || !platform::file_content_stamp_matches(receipt.1, expected_content.revision.stamp)
        || hash_parked_file(parked, expected_content.revision.size)? != expected_content.sha256
        || observe_parked_revision(
            parent,
            park_name,
            parked,
            expected_content.revision.identity,
        )
        .ok()
            != Some(receipt)
    {
        return Err(identity_changed(
            "parked file did not match its expected content receipt",
        ));
    }
    Ok(receipt)
}

pub(crate) fn verify_parked_revision(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
) -> io::Result<()> {
    if observe_parked_revision(parent, park_name, parked, identity)? != (size, stamp) {
        return Err(identity_changed("parked file revision changed"));
    }
    Ok(())
}

pub(crate) fn observe_parked_revision(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    identity: platform::Identity,
) -> io::Result<(u64, platform::FileStamp)> {
    let receipt = platform::parked_file_receipt_fields(parked)?;
    if platform::file_binding_state(parent.inner.handle(), park_name.as_os_str(), identity)?
        != platform::BindingState::Exact
    {
        return Err(identity_changed("parked file binding changed"));
    }
    Ok(receipt)
}

pub(crate) fn prove_restored_file<F>(
    request: &FileParkRequest,
    record: &FileParkSettlementRecord,
    operation: &CapabilityOperation,
    after_hash: F,
) -> io::Result<RestoredFileProof>
where
    F: FnOnce(),
{
    request.file.validate(operation)?;
    if request.file.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        || request.expected.revision.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        || request.file.identity != record.identity
        || request.expected.revision.identity != record.identity
        || platform::parked_file_identity(&record.cleanup)? != record.identity
    {
        return Err(identity_changed(
            "restored file authority changed before content proof",
        ));
    }
    let before = observe_parked_revision(
        &record.parent,
        &record.original_name,
        &record.cleanup,
        record.identity,
    )?;
    let digest = hash_parked_file(&record.cleanup, before.0)?;
    after_hash();
    request.file.validate(operation)?;
    if platform::parked_file_identity(&record.cleanup)? != record.identity {
        return Err(identity_changed(
            "restored file authority changed during content proof",
        ));
    }
    let after = observe_parked_revision(
        &record.parent,
        &record.original_name,
        &record.cleanup,
        record.identity,
    )?;
    if after != before {
        return Err(identity_changed(
            "restored file changed while its content was being proven",
        ));
    }
    if before.0 != request.expected.revision.size || digest != request.expected.sha256 {
        return Ok(RestoredFileProof::Preserved);
    }
    Ok(RestoredFileProof::Current(FileRevision {
        authority: request.file.authority.clone(),
        identity: record.identity,
        size: before.0,
        stamp: before.1,
    }))
}

pub(crate) fn finish_new_file_park(
    mut request: FileParkRequest,
    park_name: LeafName,
    mut token: FileParkRegistryToken,
    operation: &CapabilityOperation,
) -> FileParkOutcome {
    let parent = request.file.parent.clone();
    let original_name = request.file.name.clone();
    let identity = request.file.identity;
    let authority = operation.authority.clone();
    let mut guard = match authority.take_file_park(operation, &token) {
        Ok(guard) => guard,
        Err(error) => {
            return FileParkOutcome::AppliedUnverified(FileParkObligation {
                error,
                request: Some(request),
                token,
                park_name,
                phase: FileParkPhase::Parking,
                digest_verified: false,
                #[cfg(test)]
                restored_proof_pause: None,
            });
        }
    };
    match verify_parked_file(
        &parent,
        &park_name,
        &guard.record().cleanup,
        &request.expected,
    ) {
        Ok((size, stamp)) => {
            let record = guard.record_mut();
            record.size = size;
            record.stamp = stamp;
            record.expected_digest = None;
            if parent.validate(operation).is_ok() {
                record.phase = FileParkRegistryPhase::Live;
                drop(guard);
                FileParkOutcome::Parked(ParkedFile {
                    parent,
                    original_name,
                    park_name,
                    identity,
                    size,
                    stamp,
                    verified: true,
                    token,
                    authority: request.file.authority.clone(),
                })
            } else {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error: identity_changed("file park lost its authority chain"),
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::Parking,
                    digest_verified: true,
                    #[cfg(test)]
                    restored_proof_pause: None,
                })
            }
        }
        Err(error) => {
            let restoration = {
                let record = guard.record_mut();
                platform::restore_parked_file(
                    record.parent.inner.handle(),
                    record.name.as_os_str(),
                    &mut record.cleanup,
                    record.identity,
                    record.original_name.as_os_str(),
                )
            };
            match restoration {
                Ok(_) => match prove_restored_file(&request, guard.record(), operation, || {}) {
                    Ok(RestoredFileProof::Current(revision)) => {
                        request.expected.revision = revision;
                        guard.disarm(&mut token, operation);
                        FileParkOutcome::NoEffect { error, request }
                    }
                    Ok(RestoredFileProof::Preserved) => {
                        let FileParkRequest { file, expected: _ } = request;
                        guard.disarm(&mut token, operation);
                        FileParkOutcome::Preserved {
                            error: identity_changed(
                                "restored file content no longer matches the park request",
                            ),
                            file,
                        }
                    }
                    Err(proof) => {
                        drop(guard);
                        FileParkOutcome::AppliedUnverified(FileParkObligation {
                            error: proof,
                            request: Some(request),
                            token,
                            park_name,
                            phase: FileParkPhase::RestoringRejectedReceipt,
                            digest_verified: false,
                            #[cfg(test)]
                            restored_proof_pause: None,
                        })
                    }
                },
                Err(restore) => {
                    drop(guard);
                    FileParkOutcome::AppliedUnverified(FileParkObligation {
                        error: io::Error::other(format!(
                            "file receipt was rejected and restoration was not proven: {error}; {restore}"
                        )),
                        request: Some(request),
                        token,
                        park_name,
                        phase: FileParkPhase::RestoringRejectedReceipt,
                        digest_verified: false,
                        #[cfg(test)]
                        restored_proof_pause: None,
                    })
                }
            }
        }
    }
}

pub(crate) fn settle_file_park(
    mut obligation: FileParkObligation,
    force_restore: bool,
) -> FileParkResolution {
    #[cfg(test)]
    let restored_proof_pause = obligation.restored_proof_pause.take();
    let request = obligation
        .request
        .as_ref()
        .expect("park obligation retains request");
    let authority = match request.file.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    if request.file.parent.validate(&operation).is_err() {
        return FileParkResolution::Indeterminate(obligation);
    }
    let mut guard = match authority.take_file_park(&operation, &obligation.token) {
        Ok(guard) => guard,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    let original = platform::file_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().original_name.as_os_str(),
        guard.record().identity,
    );
    let parked_state = platform::file_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        guard.record().identity,
    );
    match (original, parked_state) {
        (Ok(platform::BindingState::Exact), Ok(platform::BindingState::Absent)) => {
            let proof = prove_restored_file(request, guard.record(), &operation, || {
                #[cfg(test)]
                if let Some(pause) = restored_proof_pause.as_ref() {
                    pause.hashed.wait();
                    pause.resume.wait();
                }
            });
            match proof {
                Ok(RestoredFileProof::Current(revision)) => {
                    obligation
                        .request
                        .as_mut()
                        .expect("park obligation retains request")
                        .expected
                        .revision = revision;
                    guard.disarm(&mut obligation.token, &operation);
                    FileParkResolution::NoEffect(obligation.request.take().expect("park request"))
                }
                Ok(RestoredFileProof::Preserved) => {
                    let FileParkRequest { file, expected: _ } =
                        obligation.request.take().expect("park request");
                    guard.disarm(&mut obligation.token, &operation);
                    FileParkResolution::Preserved {
                        error: identity_changed(
                            "restored file content no longer matches the park request",
                        ),
                        file,
                    }
                }
                Err(error) => {
                    obligation.error = error;
                    drop(guard);
                    FileParkResolution::Indeterminate(obligation)
                }
            }
        }
        (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
            if request
                .validate_authority_after_namespace_change(&operation)
                .is_err()
            {
                drop(guard);
                return FileParkResolution::Indeterminate(obligation);
            }
            if force_restore || obligation.phase == FileParkPhase::RestoringRejectedReceipt {
                let restoration = {
                    let record = guard.record_mut();
                    platform::restore_parked_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        &mut record.cleanup,
                        record.identity,
                        record.original_name.as_os_str(),
                    )
                };
                return match restoration {
                    Ok(_) => {
                        let proof =
                            prove_restored_file(request, guard.record(), &operation, || {
                                #[cfg(test)]
                                if let Some(pause) = restored_proof_pause.as_ref() {
                                    pause.hashed.wait();
                                    pause.resume.wait();
                                }
                            });
                        match proof {
                            Ok(RestoredFileProof::Current(revision)) => {
                                obligation
                                    .request
                                    .as_mut()
                                    .expect("park obligation retains request")
                                    .expected
                                    .revision = revision;
                                guard.disarm(&mut obligation.token, &operation);
                                FileParkResolution::NoEffect(
                                    obligation.request.take().expect("park request"),
                                )
                            }
                            Ok(RestoredFileProof::Preserved) => {
                                let FileParkRequest { file, expected: _ } =
                                    obligation.request.take().expect("park request");
                                guard.disarm(&mut obligation.token, &operation);
                                FileParkResolution::Preserved {
                                    error: identity_changed(
                                        "restored file content no longer matches the park request",
                                    ),
                                    file,
                                }
                            }
                            Err(error) => {
                                obligation.error = error;
                                drop(guard);
                                FileParkResolution::Indeterminate(obligation)
                            }
                        }
                    }
                    Err(error) => {
                        obligation.error = error;
                        drop(guard);
                        FileParkResolution::Indeterminate(obligation)
                    }
                };
            }
            if !obligation.digest_verified {
                let (size, stamp) = match verify_parked_file(
                    &request.file.parent,
                    &obligation.park_name,
                    &guard.record().cleanup,
                    &request.expected,
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        obligation.error = error;
                        obligation.phase = FileParkPhase::RestoringRejectedReceipt;
                        drop(guard);
                        return settle_file_park(obligation, true);
                    }
                };
                obligation.digest_verified = true;
                let record = guard.record_mut();
                record.size = size;
                record.stamp = stamp;
                record.expected_digest = None;
            }
            let request = obligation.request.take().expect("park request");
            let record = guard.record();
            if verify_parked_revision(
                &request.file.parent,
                &obligation.park_name,
                &record.cleanup,
                record.identity,
                record.size,
                record.stamp,
            )
            .is_err()
            {
                obligation.request = Some(request);
                return FileParkResolution::Indeterminate(obligation);
            }
            if request.file.parent.validate(&operation).is_err() {
                obligation.request = Some(request);
                return FileParkResolution::Indeterminate(obligation);
            }
            let record = guard.record_mut();
            record.phase = FileParkRegistryPhase::Live;
            let size = record.size;
            let stamp = record.stamp;
            drop(guard);
            FileParkResolution::Parked(ParkedFile {
                parent: request.file.parent.clone(),
                original_name: request.file.name.clone(),
                park_name: obligation.park_name,
                identity: request.file.identity,
                size,
                stamp,
                verified: true,
                token: obligation.token,
                authority: request.file.authority.clone(),
            })
        }
        _ => FileParkResolution::Indeterminate(obligation),
    }
}
