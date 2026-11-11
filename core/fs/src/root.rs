use super::*;

#[derive(Debug, thiserror::Error)]
pub enum RootSessionError {
    #[error("running process image could not be retained")]
    ProcessImage(#[source] io::Error),
    #[error("application root could not be created")]
    Create(#[source] io::Error),
    #[error("application root is not an exact physical directory")]
    Open(#[source] io::Error),
    #[error("application root is already leased by another process")]
    Busy,
    #[error("application root lease could not be acquired")]
    Lease(#[source] io::Error),
    #[error("application root recovery could not be initialized or replayed")]
    Recovery(#[source] io::Error),
}

#[must_use = "root acquisition effects must be explicitly acquired, reconciled, cleaned up, or preserved"]
#[derive(Debug)]
pub enum RootSessionAcquireOutcome {
    Acquired(RootSession),
    NoEffect(RootSessionError),
    AppliedUnverified(RootSessionAcquireObligation),
}

#[must_use = "absolute directory admission outcome must be handled"]
#[derive(Debug)]
pub enum AbsoluteDirectoryOutsideRootAdmission {
    Admitted(AdmittedAbsoluteDirectory),
    InsideRoot,
    Unavailable(io::Error),
}

pub(crate) struct AcquiredRoot {
    pub(crate) lease: platform::LeaseHandle,
    pub(crate) replay: Option<recovery::replay::RecoveryReplay>,
}

#[must_use = "partial root construction must be reconciled, cleaned up, or preserved"]
pub struct RootSessionAcquireObligation {
    pub(crate) error: RootSessionError,
    pub(crate) construction: Option<platform::RootConstruction>,
    pub(crate) lease: Option<platform::LeaseAcquisitionObligation>,
    pub(crate) acquired: Option<Box<AcquiredRoot>>,
    pub(crate) process_image: Option<platform::ProcessImageAncestry>,
    pub(crate) admission: Option<Arc<AdmittedAbsoluteDirectoryInner>>,
}

impl_redacted_debug!(RootSessionAcquireObligation);

impl RootSessionAcquireObligation {
    pub fn error(&self) -> &RootSessionError {
        &self.error
    }

    pub fn reconcile(mut self) -> RootSessionAcquireOutcome {
        let admission = self.admission.take();
        match self.reconcile_construction() {
            RootSessionAcquireOutcome::Acquired(mut session) => {
                session.admission = admission;
                RootSessionAcquireOutcome::Acquired(session)
            }
            RootSessionAcquireOutcome::AppliedUnverified(mut obligation) => {
                obligation.admission = admission;
                RootSessionAcquireOutcome::AppliedUnverified(obligation)
            }
            outcome @ RootSessionAcquireOutcome::NoEffect(_) => outcome,
        }
    }

    pub(crate) fn reconcile_construction(mut self) -> RootSessionAcquireOutcome {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(mut acquired) = self.acquired.take() {
            let identity = match platform::root_construction_identity(&construction) {
                Ok(identity) => identity,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.acquired = Some(acquired);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            let process_image = self
                .process_image
                .take()
                .expect("root acquisition obligation retains process image ancestry");
            if let Some(replay) = acquired.replay.take() {
                let root = match platform::root_construction_guard(&construction) {
                    Ok(root) => root,
                    Err(error) => {
                        self.error = RootSessionError::Create(error);
                        acquired.replay = Some(replay);
                        self.construction = Some(construction);
                        self.acquired = Some(acquired);
                        self.process_image = Some(process_image);
                        return RootSessionAcquireOutcome::AppliedUnverified(self);
                    }
                };
                return match replay.resume(root, &acquired.lease) {
                    Ok(recovery) => finish_root_session_with_recovery(
                        construction,
                        identity,
                        acquired.lease,
                        process_image,
                        recovery,
                    ),
                    Err((error, replay)) => {
                        self.error = RootSessionError::Recovery(error);
                        acquired.replay = Some(replay);
                        self.construction = Some(construction);
                        self.acquired = Some(acquired);
                        self.process_image = Some(process_image);
                        RootSessionAcquireOutcome::AppliedUnverified(self)
                    }
                };
            }
            return finish_root_session(construction, identity, acquired.lease, process_image);
        }
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            let identity = match platform::root_construction_identity(&construction) {
                Ok(identity) => identity,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            match platform::reconcile_lease_acquisition(root, lease) {
                Ok(lease) => {
                    let process_image = self
                        .process_image
                        .take()
                        .expect("root acquisition obligation retains process image ancestry");
                    return finish_root_session(construction, identity, lease, process_image);
                }
                Err(lease) => {
                    self.error = RootSessionError::Lease(copy_io_error(
                        platform::lease_acquisition_error(&lease),
                    ));
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            }
        }
        match platform::reconcile_root_construction(construction) {
            Ok(construction) => {
                let process_image = self
                    .process_image
                    .take()
                    .expect("root acquisition obligation retains process image ancestry");
                try_acquire_lease_and_finish_root(construction, process_image)
            }
            Err(error) => {
                let (error, construction) = error.into_parts();
                self.error = RootSessionError::Create(error);
                self.construction = construction;
                RootSessionAcquireOutcome::AppliedUnverified(self)
            }
        }
    }

    pub fn cleanup(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if self.acquired.is_some() {
            self.construction = Some(construction);
            return Err(self);
        }
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return Err(self);
                }
            };
            let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
            if let Err(lease) =
                platform::cleanup_lease_acquisition(root, lease_name.as_os_str(), lease)
            {
                self.error = RootSessionError::Lease(copy_io_error(
                    platform::lease_acquisition_error(&lease),
                ));
                self.construction = Some(construction);
                self.lease = Some(lease);
                return Err(self);
            }
        }
        match platform::cleanup_root_construction(construction) {
            Ok(()) => Ok(()),
            Err(error) => {
                let (error, construction) = error.into_parts();
                self.error = RootSessionError::Create(error);
                self.construction = construction;
                Err(self)
            }
        }
    }

    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(mut acquired) = self.acquired.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.acquired = Some(acquired);
                    return Err(self);
                }
            };
            if let Err(error) = platform::validate_lease(&acquired.lease)
                .and_then(|()| platform::validate_root(root))
            {
                self.error = RootSessionError::Recovery(error);
                self.construction = Some(construction);
                self.acquired = Some(acquired);
                return Err(self);
            }
            let root = platform::finish_root_construction(construction);
            if let Some(replay) = acquired.replay.take() {
                replay.acknowledge();
            }
            drop(self.process_image.take());
            drop(acquired.lease);
            drop(root);
            return Ok(());
        }
        if !platform::root_construction_has_unclassified(&construction) {
            self.construction = Some(construction);
            return Err(self);
        }
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return Err(self);
                }
            };
            let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
            if let Err(lease) =
                platform::cleanup_lease_acquisition(root, lease_name.as_os_str(), lease)
            {
                self.error = RootSessionError::Lease(copy_io_error(
                    platform::lease_acquisition_error(&lease),
                ));
                self.construction = Some(construction);
                self.lease = Some(lease);
                return Err(self);
            }
        }
        platform::acknowledge_preserved_root_construction(construction);
        Ok(())
    }
}

impl Drop for RootSessionAcquireObligation {
    fn drop(&mut self) {
        if self.construction.is_some() || self.lease.is_some() || self.acquired.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "a root session must be explicitly revoked or transferred into reset"]
pub struct RootSession {
    pub(crate) root_identity: DirectoryIdentity,
    pub(crate) authority: Arc<CapabilityAuthority>,
    /// The absolute directory admission this root was acquired through, revalidated
    /// around every root and directory admission.
    pub(crate) admission: Option<Arc<AdmittedAbsoluteDirectoryInner>>,
}

pub(crate) struct SessionDrainRecoveryState {
    pub(crate) files: Vec<ParkedFile>,
    pub(crate) directories: Vec<ParkedDirectory>,
    pub(crate) directory_create_preservations: Vec<DirectoryCreatePreservation>,
    pub(crate) file_removals: Vec<FileRemovalObligation>,
    pub(crate) file_restores: Vec<FileRestoreObligation>,
    pub(crate) directory_removals: Vec<DirectoryRemovalObligation>,
    pub(crate) directory_restores: Vec<DirectoryRestoreObligation>,
}

impl_redacted_debug!(SessionDrainRecoveryState);

#[derive(Clone, Copy)]
pub(crate) enum DrainRecoveryDecision {
    Remove,
    Restore,
}

impl SessionDrainRecoveryState {
    pub(crate) fn file_count(&self) -> usize {
        self.files
            .len()
            .saturating_add(self.file_removals.len())
            .saturating_add(self.file_restores.len())
    }

    pub(crate) fn directory_count(&self) -> usize {
        self.directories
            .len()
            .saturating_add(self.directory_removals.len())
            .saturating_add(self.directory_restores.len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.directories.is_empty()
            && self.directory_create_preservations.is_empty()
            && self.file_removals.is_empty()
            && self.file_restores.is_empty()
            && self.directory_removals.is_empty()
            && self.directory_restores.is_empty()
    }

    pub(crate) fn settle(
        &mut self,
        permits: &[DrainRecoveryPermit],
        decision: DrainRecoveryDecision,
    ) {
        let permit_by_park = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();

        for obligation in std::mem::take(&mut self.file_removals) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("file removal recovery retains parked file")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(token_id)) else {
                self.file_removals.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                FileRemovalResolution::Applied(()) => {}
                FileRemovalResolution::NoEffect(parked) => {
                    self.files.push(parked);
                }
                FileRemovalResolution::Indeterminate(obligation) => {
                    self.file_removals.push(obligation);
                }
            };
        }
        for obligation in std::mem::take(&mut self.file_restores) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("file restore recovery retains parked file")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(token_id)) else {
                self.file_restores.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                FileRestoreResolution::Applied(_) => {}
                FileRestoreResolution::NoEffect(parked) => {
                    self.files.push(parked);
                }
                FileRestoreResolution::Indeterminate(obligation) => {
                    self.file_restores.push(obligation);
                }
            };
        }

        for parked in std::mem::take(&mut self.files) {
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(parked.token.id))
            else {
                self.files.push(parked);
                continue;
            };
            match decision {
                DrainRecoveryDecision::Remove => match parked.remove_with_recovery(permit) {
                    FileRemovalOutcome::Applied(()) => {}
                    FileRemovalOutcome::NoEffect { item: parked, .. } => self.files.push(parked),
                    FileRemovalOutcome::AppliedUnverified(obligation) => {
                        self.file_removals.push(obligation);
                    }
                },
                DrainRecoveryDecision::Restore => match parked.restore_with_recovery(permit) {
                    FileRestoreOutcome::Applied(_) => {}
                    FileRestoreOutcome::NoEffect { item: parked, .. } => self.files.push(parked),
                    FileRestoreOutcome::AppliedUnverified(obligation) => {
                        self.file_restores.push(obligation);
                    }
                },
            }
        }

        for obligation in std::mem::take(&mut self.directory_removals) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("directory removal recovery retains parked directory")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(token_id)) else {
                self.directory_removals.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                DirectoryRemovalResolution::Applied(()) => {}
                DirectoryRemovalResolution::NoEffect(parked) => {
                    self.directories.push(parked);
                }
                DirectoryRemovalResolution::Indeterminate(obligation) => {
                    self.directory_removals.push(obligation);
                }
            };
        }
        for obligation in std::mem::take(&mut self.directory_restores) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("directory restore recovery retains parked directory")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(token_id)) else {
                self.directory_restores.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                DirectoryRestoreResolution::Applied(_) => {}
                DirectoryRestoreResolution::NoEffect(parked) => {
                    self.directories.push(parked);
                }
                DirectoryRestoreResolution::Indeterminate(obligation) => {
                    self.directory_restores.push(obligation);
                }
            };
        }

        for parked in std::mem::take(&mut self.directories) {
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(parked.token.id))
            else {
                self.directories.push(parked);
                continue;
            };
            match decision {
                DrainRecoveryDecision::Remove => match parked.remove_empty_with_recovery(permit) {
                    DirectoryRemovalOutcome::Applied(()) => {}
                    DirectoryRemovalOutcome::NoEffect { item: parked, .. } => {
                        self.directories.push(parked);
                    }
                    DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
                        self.directory_removals.push(obligation);
                    }
                },
                DrainRecoveryDecision::Restore => match parked.restore_with_recovery(permit) {
                    DirectoryRestoreOutcome::Applied(_) => {}
                    DirectoryRestoreOutcome::NoEffect { item: parked, .. } => {
                        self.directories.push(parked);
                    }
                    DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                        self.directory_restores.push(obligation);
                    }
                },
            }
        }
    }

    pub(crate) fn acknowledge_external_directory_creates(
        &mut self,
        permits: &[DrainRecoveryPermit],
    ) {
        let permit_by_effect = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();
        for preservation in std::mem::take(&mut self.directory_create_preservations) {
            let token_id = preservation.token.id;
            let Some(permit) =
                permit_by_effect.get(&DrainRecoveryParkId::DirectoryCreate(token_id))
            else {
                self.directory_create_preservations.push(preservation);
                continue;
            };
            if !permit
                .authority
                .directory_create_is_external(&preservation.token)
            {
                self.directory_create_preservations.push(preservation);
                continue;
            }
            if let Err(preservation) = preservation.acknowledge_preserved_with_recovery(permit) {
                self.directory_create_preservations.push(preservation);
            }
        }
    }

    pub(crate) fn transfer_preserved_directory_creates_to_reset(
        &mut self,
        permits: &[DrainRecoveryPermit],
    ) {
        let permit_by_effect = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();
        for preservation in std::mem::take(&mut self.directory_create_preservations) {
            let token_id = preservation.token.id;
            let Some(permit) =
                permit_by_effect.get(&DrainRecoveryParkId::DirectoryCreate(token_id))
            else {
                self.directory_create_preservations.push(preservation);
                continue;
            };
            if let Err(preservation) = preservation.transfer_to_reset(permit) {
                self.directory_create_preservations.push(preservation);
            }
        }
    }
}

impl_redacted_debug!(RootSession);

impl RootSession {
    pub fn acquire(path: &Path) -> RootSessionAcquireOutcome {
        let process_image = match capture_process_image_ancestry() {
            Ok(process_image) => process_image,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::ProcessImage(error));
            }
        };
        match platform::open_or_create_root(path) {
            Ok(construction) => try_acquire_lease_and_finish_root(construction, process_image),
            Err(error) => {
                let (error, construction) = error.into_parts();
                match construction {
                    Some(construction) => {
                        RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                            error: RootSessionError::Create(error),
                            construction: Some(construction),
                            lease: None,
                            acquired: None,
                            process_image: Some(process_image),
                            admission: None,
                        })
                    }
                    None => RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error)),
                }
            }
        }
    }

    pub(crate) fn acquire_absolute_directory_guard(
        guard: &platform::AbsoluteDirectoryGuard,
    ) -> RootSessionAcquireOutcome {
        let process_image = match capture_process_image_ancestry() {
            Ok(process_image) => process_image,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::ProcessImage(error));
            }
        };
        let construction = match platform::root_construction_from_absolute_directory_guard(guard) {
            Ok(construction) => construction,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error));
            }
        };
        try_acquire_lease_and_finish_root(construction, process_image)
    }

    pub fn identity(&self) -> DirectoryIdentity {
        self.root_identity
    }

    pub fn root(&self) -> io::Result<Directory> {
        self.validate_admission()?;
        let operation = self.authority.enter()?;
        let root = Directory::from_handle(
            platform::clone_root(&self.authority.root)?,
            self.root_identity,
            Arc::downgrade(&self.authority),
            None,
        );
        root.validate(&operation)?;
        self.validate_admission()?;
        Ok(root)
    }

    pub(crate) fn validate_admission(&self) -> io::Result<()> {
        let Some(admission) = &self.admission else {
            return Ok(());
        };
        let admitted = &admission.directory.inner.identity;
        if platform::directory_identity(admission.directory.inner.handle())? != admitted.physical
            || admitted.filesystem_identity() != self.root_identity.filesystem_identity()
        {
            return Err(identity_changed(
                "admitted root session changed physical identity",
            ));
        }
        Ok(())
    }

    pub(crate) fn admit_absolute_directory_authority(
        &self,
        path: &Path,
    ) -> io::Result<AdmittedAbsoluteDirectory> {
        let directory = self.admit_absolute_directory_capability(path)?;
        let admitted = AdmittedAbsoluteDirectory {
            inner: Arc::new(AdmittedAbsoluteDirectoryInner { directory }),
        };
        admitted.revalidate()?;
        Ok(admitted)
    }

    pub fn admit_absolute_directory_authority_outside_root(
        &self,
        path: &Path,
    ) -> AbsoluteDirectoryOutsideRootAdmission {
        let admitted = match self.admit_absolute_directory_authority(path) {
            Ok(admitted) => admitted,
            Err(error) => {
                return AbsoluteDirectoryOutsideRootAdmission::Unavailable(error);
            }
        };
        let Some(guard) = admitted.inner.directory.inner.absolute_ancestry() else {
            return AbsoluteDirectoryOutsideRootAdmission::Unavailable(io::Error::other(
                "absolute directory admission lost its ancestry",
            ));
        };
        match platform::absolute_directory_is_outside_root(guard, &self.authority.root) {
            Ok(true) => AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted),
            Ok(false) => AbsoluteDirectoryOutsideRootAdmission::InsideRoot,
            Err(error) => AbsoluteDirectoryOutsideRootAdmission::Unavailable(error),
        }
    }

    pub fn admit_absolute_directory(&self, path: &Path) -> io::Result<Directory> {
        self.admit_absolute_directory_capability(path)
    }

    pub(crate) fn admit_absolute_directory_capability(&self, path: &Path) -> io::Result<Directory> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external directory is not absolute",
            ));
        }
        self.validate_admission()?;
        let operation = self.authority.enter()?;
        let ancestry: platform::AbsoluteDirectoryGuard =
            platform::open_absolute_directory_guard(path)?;
        let directory = Directory::from_absolute_guard(ancestry, &self.authority);
        directory.validate(&operation)?;
        self.validate_admission()?;
        Ok(directory)
    }

    pub fn validate_reset_preflight(&self) -> io::Result<()> {
        self.authority
            .validate_retained_process_image_outside_root()?;
        platform::validate_lease(&self.authority.lease)?;
        platform::validate_root(&self.authority.root)
    }

    pub fn validate_retained_authority(&self) -> io::Result<()> {
        platform::validate_lease(&self.authority.lease)?;
        platform::validate_root_handle(&self.authority.root)?;
        self.validate_admission()
    }

    pub fn revoke(self) -> RootRevokeOutcome {
        let start = self.authority.begin_terminal_drain(false);
        match start {
            Ok(()) => RootRevokeDrain {
                session: Some(self),
            }
            .try_settle(),
            Err(error) => RootRevokeOutcome::Refused(RootRevokeStartFailure::new(self, error)),
        }
    }

    pub(crate) fn begin_reset(self) -> ResetStartOutcome {
        if let Err(error) = self
            .authority
            .validate_retained_process_image_outside_root()
        {
            return ResetStartOutcome::Refused(ResetStartFailure::new(self, error));
        }
        let start = self.authority.begin_terminal_drain(true);
        match start {
            Ok(()) => ResetDrainAuthority {
                session: Some(self),
            }
            .try_settle(),
            Err(error) => ResetStartOutcome::Refused(ResetStartFailure::new(self, error)),
        }
    }
}

pub(crate) fn capture_process_image_ancestry() -> io::Result<platform::ProcessImageAncestry> {
    let executable = std::env::current_exe()?;
    platform::capture_process_image_ancestry(&executable)
}

pub(crate) fn try_acquire_lease_and_finish_root(
    construction: platform::RootConstruction,
    process_image: platform::ProcessImageAncestry,
) -> RootSessionAcquireOutcome {
    let identity = match platform::root_construction_identity(&construction) {
        Ok(identity) => identity,
        Err(error) => {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Create(error),
                    construction: Some(construction),
                    lease: None,
                    acquired: None,
                    process_image: Some(process_image),
                    admission: None,
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error))
            };
        }
    };
    let root = match platform::root_construction_guard(&construction) {
        Ok(root) => root,
        Err(error) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Create(error),
                construction: Some(construction),
                lease: None,
                acquired: None,
                process_image: Some(process_image),
                admission: None,
            });
        }
    };
    let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
    let lease = match platform::try_acquire_lease(root, lease_name.as_os_str()) {
        platform::LeaseAcquisitionOutcome::Acquired(lease) => lease,
        platform::LeaseAcquisitionOutcome::NoEffect(error)
            if error.kind() == io::ErrorKind::WouldBlock =>
        {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Busy,
                    construction: Some(construction),
                    lease: None,
                    acquired: None,
                    process_image: Some(process_image),
                    admission: None,
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
            };
        }
        platform::LeaseAcquisitionOutcome::NoEffect(error) => {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Lease(error),
                    construction: Some(construction),
                    lease: None,
                    acquired: None,
                    process_image: Some(process_image),
                    admission: None,
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Lease(error))
            };
        }
        platform::LeaseAcquisitionOutcome::AppliedUnverified(lease) => {
            let error =
                RootSessionError::Lease(copy_io_error(platform::lease_acquisition_error(&lease)));
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error,
                construction: Some(construction),
                lease: Some(lease),
                acquired: None,
                process_image: Some(process_image),
                admission: None,
            });
        }
    };
    finish_root_session(construction, identity, lease, process_image)
}

pub(crate) fn finish_root_session(
    construction: platform::RootConstruction,
    identity: platform::Identity,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
) -> RootSessionAcquireOutcome {
    let root = match platform::root_construction_guard(&construction) {
        Ok(root) => root,
        Err(error) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Create(error),
                construction: Some(construction),
                lease: None,
                acquired: Some(Box::new(AcquiredRoot {
                    lease,
                    replay: None,
                })),
                process_image: Some(process_image),
                admission: None,
            });
        }
    };
    let recovery = match recovery::replay::initialize_and_replay(root, &lease) {
        Ok(recovery) => recovery,
        Err((error, replay)) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Recovery(error),
                construction: Some(construction),
                lease: None,
                acquired: Some(Box::new(AcquiredRoot { lease, replay })),
                process_image: Some(process_image),
                admission: None,
            });
        }
    };
    finish_root_session_with_recovery(construction, identity, lease, process_image, recovery)
}

pub(crate) fn finish_root_session_with_recovery(
    construction: platform::RootConstruction,
    identity: platform::Identity,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
    (recovery, recovery_orphans): (RecoveryJournal, Vec<RecoveryOrphan>),
) -> RootSessionAcquireOutcome {
    use rand::RngCore;

    let root = platform::finish_root_construction(construction);
    let mut session_nonce = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut session_nonce);
    RootSessionAcquireOutcome::Acquired(RootSession {
        admission: None,
        root_identity: DirectoryIdentity {
            session: session_nonce,
            physical: identity,
        },
        authority: Arc::new(CapabilityAuthority {
            operations: Mutex::new(OperationState {
                phase: AUTHORITY_LIVE,
                root_identity: identity,
                active: 0,
                outstanding_effects: 0,
                next_id: 1,
                moves: HashMap::new(),
                effect_owner_handles: HashMap::new(),
                active_effect_owners: HashMap::new(),
                stages: HashMap::new(),
                stage_creations: HashMap::new(),
                directory_creations: HashMap::new(),
                file_parks: HashMap::new(),
                directory_parks: HashMap::new(),
                transients: HashMap::new(),
                recovery,
                recovery_orphans,
            }),
            #[cfg(test)]
            directory_open_pause: Mutex::new(None),
            #[cfg(test)]
            file_observation_pause: Mutex::new(None),
            #[cfg(test)]
            revision_pause: Mutex::new(None),
            session_nonce,
            root,
            lease,
            process_image,
        }),
    })
}

impl Drop for RootSession {
    fn drop(&mut self) {
        let phase = match self.authority.operations.lock() {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        }
        .phase;
        if phase == AUTHORITY_LIVE && self.authority.begin_terminal_drain(false).is_err() {
            std::process::abort();
        }
        let state = match self.authority.operations.lock() {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        };
        if state.active != 0
            || state.outstanding_effects != 0
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !state.directory_creations.is_empty()
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.transients.is_empty()
            || state.recovery.is_checked_out()
            || !state.active_effect_owners.is_empty()
            || matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_QUIESCING)
        {
            std::process::abort();
        }
    }
}

#[must_use = "revocation outcome retains linear session authority until it is terminal"]
#[derive(Debug)]
pub enum RootRevokeOutcome {
    Revoked,
    Pending(RootRevokeDrain),
    Recovery { recovery: RootRevokeRecovery },
    Refused(RootRevokeStartFailure),
    Failed(RootRevokeDrainFailure),
}

#[must_use = "revocation drain must be settled or transferred with recovery authority"]
pub struct RootRevokeDrain {
    pub(crate) session: Option<RootSession>,
}

impl_redacted_debug!(RootRevokeDrain);

impl RootRevokeDrain {
    pub fn try_settle(mut self) -> RootRevokeOutcome {
        let session = self.session.take().expect("revoke drain retains session");
        let authority = session.authority.clone();
        let settlement = authority.try_finish_terminal_drain(AUTHORITY_REVOKED, false);
        match settlement {
            Ok(SessionDrainSettlement::Ready) => RootRevokeOutcome::Revoked,
            Ok(SessionDrainSettlement::Pending) => {
                self.session = Some(session);
                RootRevokeOutcome::Pending(self)
            }
            Ok(SessionDrainSettlement::Recovery { recovery, permits }) => {
                self.session = Some(session);
                RootRevokeOutcome::Recovery {
                    recovery: RootRevokeRecovery {
                        drain: self,
                        recovery,
                        permits,
                    },
                }
            }
            Err(error) => {
                self.session = Some(session);
                RootRevokeOutcome::Failed(RootRevokeDrainFailure::new(self, error))
            }
        }
    }
}

#[must_use = "revocation recovery must settle every abandoned effect before the drain can finish"]
pub struct RootRevokeRecovery {
    pub(crate) drain: RootRevokeDrain,
    pub(crate) recovery: SessionDrainRecoveryState,
    pub(crate) permits: Vec<DrainRecoveryPermit>,
}

impl_redacted_debug!(RootRevokeRecovery);

impl RootRevokeRecovery {
    pub fn file_count(&self) -> usize {
        self.recovery.file_count()
    }

    pub fn directory_count(&self) -> usize {
        self.recovery.directory_count()
    }

    pub fn restore_all(mut self) -> RootRevokeOutcome {
        self.recovery
            .settle(&self.permits, DrainRecoveryDecision::Restore);
        self.finish()
    }

    pub(crate) fn finish(self) -> RootRevokeOutcome {
        if self.recovery.is_empty() {
            self.drain.try_settle()
        } else {
            RootRevokeOutcome::Recovery { recovery: self }
        }
    }
}

#[must_use = "revocation start refusal retains the live session and must be retried"]
pub struct RootRevokeStartFailure {
    pub(crate) error: io::Error,
    pub(crate) session: Option<RootSession>,
}

impl_redacted_debug!(RootRevokeStartFailure);

impl RootRevokeStartFailure {
    pub(crate) fn new(session: RootSession, error: io::Error) -> Self {
        Self {
            error,
            session: Some(session),
        }
    }

    pub fn retry(mut self) -> RootRevokeOutcome {
        self.session
            .take()
            .expect("revocation start refusal retains session")
            .revoke()
    }
}

impl_error_accessor!(RootRevokeStartFailure);

#[must_use = "revocation drain failure retains the draining authority and must be retried"]
pub struct RootRevokeDrainFailure {
    pub(crate) error: io::Error,
    pub(crate) drain: Option<RootRevokeDrain>,
}

impl_redacted_debug!(RootRevokeDrainFailure);

impl RootRevokeDrainFailure {
    pub(crate) fn new(drain: RootRevokeDrain, error: io::Error) -> Self {
        Self {
            error,
            drain: Some(drain),
        }
    }

    pub fn retry(mut self) -> RootRevokeOutcome {
        self.drain
            .take()
            .expect("revocation failure retains drain")
            .try_settle()
    }
}

impl_error_accessor!(RootRevokeDrainFailure);

#[must_use = "reset outcome retains linear session authority until it is terminal"]
#[derive(Debug)]
pub enum ResetStartOutcome {
    Ready(RootResetAuthority),
    Pending(ResetDrainAuthority),
    Recovery { recovery: ResetDrainRecovery },
    Refused(ResetStartFailure),
    Failed(ResetDrainFailure),
}

#[must_use = "reset drain must be settled or transferred with recovery authority"]
pub struct ResetDrainAuthority {
    pub(crate) session: Option<RootSession>,
}

impl_redacted_debug!(ResetDrainAuthority);

impl ResetDrainAuthority {
    pub fn try_settle(mut self) -> ResetStartOutcome {
        let session = self.session.take().expect("reset drain retains session");
        let authority = session.authority.clone();
        let settlement = authority.try_finish_terminal_drain(AUTHORITY_RESETTING, true);
        match settlement {
            Ok(SessionDrainSettlement::Ready) => ResetStartOutcome::Ready(RootResetAuthority {
                session: Some(session),
            }),
            Ok(SessionDrainSettlement::Pending) => {
                self.session = Some(session);
                ResetStartOutcome::Pending(self)
            }
            Ok(SessionDrainSettlement::Recovery { recovery, permits }) => {
                self.session = Some(session);
                ResetStartOutcome::Recovery {
                    recovery: ResetDrainRecovery {
                        drain: self,
                        recovery,
                        permits,
                    },
                }
            }
            Err(error) => {
                self.session = Some(session);
                ResetStartOutcome::Failed(ResetDrainFailure::new(self, error))
            }
        }
    }
}

#[must_use = "reset recovery must settle every abandoned effect before the drain can finish"]
pub struct ResetDrainRecovery {
    pub(crate) drain: ResetDrainAuthority,
    pub(crate) recovery: SessionDrainRecoveryState,
    pub(crate) permits: Vec<DrainRecoveryPermit>,
}

impl_redacted_debug!(ResetDrainRecovery);

impl ResetDrainRecovery {
    /// Removes abandoned parks: the reset deletes the tree anyway. Once none remain,
    /// externally created directories are acknowledged and the rest move to the reset.
    pub(crate) fn settle(mut self) -> ResetStartOutcome {
        if self.recovery.file_count() > 0 || self.recovery.directory_count() > 0 {
            self.recovery
                .settle(&self.permits, DrainRecoveryDecision::Remove);
        } else {
            self.recovery
                .acknowledge_external_directory_creates(&self.permits);
            if !self.recovery.is_empty() {
                self.recovery
                    .transfer_preserved_directory_creates_to_reset(&self.permits);
            }
        }
        self.finish()
    }

    pub(crate) fn finish(self) -> ResetStartOutcome {
        if self.recovery.is_empty() {
            self.drain.try_settle()
        } else {
            ResetStartOutcome::Recovery { recovery: self }
        }
    }
}

#[must_use = "reset start failure retains the sole session and must be retried or explicitly cancelled"]
pub struct ResetStartFailure {
    pub(crate) error: io::Error,
    pub(crate) session: Option<RootSession>,
}

impl_redacted_debug!(ResetStartFailure);

impl ResetStartFailure {
    pub(crate) fn new(session: RootSession, error: io::Error) -> Self {
        Self {
            error,
            session: Some(session),
        }
    }

    pub fn retry(mut self) -> ResetStartOutcome {
        self.session
            .take()
            .expect("reset start refusal retains session")
            .begin_reset()
    }
}

impl_error_accessor!(ResetStartFailure);

impl Drop for ResetStartFailure {
    fn drop(&mut self) {
        if self.session.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "reset drain failure retains authority and must be retried or cancelled into revocation"]
pub struct ResetDrainFailure {
    pub(crate) error: io::Error,
    pub(crate) drain: Option<ResetDrainAuthority>,
}

impl_redacted_debug!(ResetDrainFailure);

impl ResetDrainFailure {
    pub(crate) fn new(drain: ResetDrainAuthority, error: io::Error) -> Self {
        Self {
            error,
            drain: Some(drain),
        }
    }

    pub fn retry(mut self) -> ResetStartOutcome {
        self.drain
            .take()
            .expect("reset failure retains drain")
            .try_settle()
    }
}

impl_error_accessor!(ResetDrainFailure);

impl Drop for ResetDrainFailure {
    fn drop(&mut self) {
        if self.drain.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "reset authority must be explicitly cleared, preserved, or released"]
pub struct RootResetAuthority {
    pub(crate) session: Option<RootSession>,
}

impl_redacted_debug!(RootResetAuthority);

impl RootResetAuthority {
    pub(crate) fn try_clear_root(
        &self,
        preserved: &[(&OsStr, platform::Identity)],
    ) -> io::Result<()> {
        let session = self.session.as_ref().expect("reset session is retained");
        let operation = session.authority.enter_reset_operation()?;
        platform::validate_lease(&session.authority.lease)?;
        platform::validate_root(&session.authority.root)?;
        let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
        platform::validate_process_image_outside_root(
            &session.authority.process_image,
            &session.authority.root,
        )?;
        platform::clear_root_children(
            &session.authority.root,
            &session.authority.lease,
            lease_name.as_os_str(),
            preserved,
        )?;
        session
            .authority
            .retire_reset_pending_directory_creates(&operation)
    }

    pub fn release(mut self) -> Result<(), Self> {
        if self
            .session
            .as_ref()
            .expect("reset session is retained")
            .authority
            .has_reset_pending_directory_creates()
        {
            return Err(self);
        }
        self.revoke();
        drop(self.session.take());
        Ok(())
    }

    pub(crate) fn revoke(&self) {
        if let Some(session) = &self.session {
            let mut state = session
                .authority
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.phase = AUTHORITY_REVOKED;
        }
    }
}

impl Drop for RootResetAuthority {
    fn drop(&mut self) {
        if self.session.is_some() {
            std::process::abort();
        }
    }
}
