use super::*;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DirectoryIdentity {
    pub(crate) session: [u8; 16],
    pub(crate) physical: platform::Identity,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DirectoryFilesystemIdentity(pub(crate) platform::Identity);

impl_redacted_debug!(DirectoryFilesystemIdentity);

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DirectoryRevision {
    pub(crate) identity: DirectoryIdentity,
    pub(crate) stamp: platform::DirectoryStamp,
}

impl_redacted_debug!(DirectoryRevision);

impl_redacted_debug!(DirectoryIdentity);

impl DirectoryIdentity {
    /// Stable comparison evidence only. This never reconstructs a capability.
    /// Reopen through retained authority before comparing a persisted witness.
    pub fn filesystem_witness(self) -> [u8; 32] {
        platform::identity_witness(self.physical)
    }

    pub fn same_filesystem_object(self, other: Self) -> bool {
        self.physical == other.physical
    }

    pub fn filesystem_identity(self) -> DirectoryFilesystemIdentity {
        DirectoryFilesystemIdentity(self.physical)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Link,
    Other,
}

#[derive(Clone)]
pub struct DirectoryEntry {
    pub(crate) name: OsString,
    pub(crate) kind: EntryKind,
    pub(crate) parent: DirectoryIdentity,
}

impl fmt::Debug for DirectoryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryEntry")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl DirectoryEntry {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn utf8_name(&self) -> Option<&str> {
        self.name.to_str()
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryListingState {
    Complete,
    Truncated,
}

#[derive(Debug)]
pub struct DirectoryListing {
    pub(crate) entries: Vec<DirectoryEntry>,
    pub(crate) state: DirectoryListingState,
}

impl DirectoryListing {
    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    pub fn state(&self) -> DirectoryListingState {
        self.state
    }
}

#[must_use = "directory creation effects must be explicitly settled or preserved"]
#[derive(Debug)]
pub enum DirectoryCreateOutcome {
    Created(Directory),
    NoEffect(io::Error),
    CreatedUnclassified {
        error: io::Error,
        preservation: DirectoryCreatePreservation,
    },
    AppliedUnverified(DirectoryCreateObligation),
}

#[must_use = "an unclassified created directory must be explicitly acknowledged as preserved"]
pub struct DirectoryCreatePreservation {
    pub(crate) token: DirectoryCreateEffectToken,
}

impl_redacted_debug!(DirectoryCreatePreservation);

impl DirectoryCreatePreservation {
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        let operation = match authority.enter_directory_rebinding() {
            Ok(operation) => operation,
            Err(_) => return Err(self),
        };
        match authority.acknowledge_unclassified_directory_create(&operation, &mut self.token) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }

    pub(crate) fn acknowledge_preserved_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        let operation = match authority.enter_directory_create_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(_) => return Err(self),
        };
        match authority.acknowledge_unclassified_directory_create(&operation, &mut self.token) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }

    pub(crate) fn transfer_to_reset(mut self, permit: &DrainRecoveryPermit) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        match authority.transfer_unclassified_directory_create_to_reset(permit, &mut self.token) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }
}

#[must_use = "directory create obligations must be reconciled"]
pub struct DirectoryCreateObligation {
    pub(crate) error: io::Error,
    pub(crate) token: DirectoryCreateEffectToken,
}

impl_redacted_debug!(DirectoryCreateObligation);

impl DirectoryCreateObligation {
    pub fn reconcile(mut self) -> DirectoryCreateResolution {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return DirectoryCreateResolution::Indeterminate(self),
        };
        let operation = match authority.enter_directory_rebinding() {
            Ok(operation) => operation,
            Err(_) => return DirectoryCreateResolution::Indeterminate(self),
        };
        match finish_directory_create(&authority, &operation, &mut self.token) {
            Ok(directory) => DirectoryCreateResolution::Created(directory),
            Err(_) => DirectoryCreateResolution::Indeterminate(self),
        }
    }
}

impl_error_accessor!(DirectoryCreateObligation);

pub(crate) fn finish_directory_create(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    reservation: &mut DirectoryCreateEffectToken,
) -> io::Result<Directory> {
    let mut guard = authority.take_directory_create(operation, reservation)?;
    if guard.record().phase != DirectoryCreateEffectPhase::Applied {
        return Err(io::Error::other(
            "directory create effect is not yet classified",
        ));
    }
    guard.record().parent.validate(operation)?;
    let created = guard
        .record()
        .created
        .as_ref()
        .ok_or_else(|| io::Error::other("directory create authority is not retained"))?;
    let identity = platform::directory_identity(created)?;
    if platform::directory_binding_state(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
        identity,
    )? != platform::BindingState::Exact
    {
        return Err(identity_changed("created directory binding is not exact"));
    }
    let (ordinary, ordinary_identity) = platform::open_directory(
        guard.record().parent.inner.handle(),
        guard.record().name.as_os_str(),
    )?;
    if ordinary_identity != identity {
        return Err(identity_changed(
            "created directory changed before least-authority admission",
        ));
    }
    let parent = guard.record().parent.clone();
    let name = guard.record().name.clone();
    let directory = Directory::from_handle(
        ordinary,
        authority.identity(identity),
        Arc::downgrade(authority),
        Some(DirectoryParent {
            directory: parent,
            name: name.as_os_str().to_os_string(),
        }),
    );
    directory.validate(operation)?;
    drop(guard.record_mut().created.take());
    guard.disarm(reservation, operation);
    Ok(directory)
}

#[must_use = "directory create resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryCreateResolution {
    Created(Directory),
    Indeterminate(DirectoryCreateObligation),
}

pub(crate) struct DirectoryInner {
    pub(crate) storage: DirectoryStorage,
    pub(crate) identity: DirectoryIdentity,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) parent: Option<DirectoryParent>,
}

pub(crate) enum DirectoryStorage {
    Owned(platform::DirectoryHandle),
    Absolute(platform::AbsoluteDirectoryGuard),
}

impl DirectoryInner {
    pub(crate) fn handle(&self) -> &platform::DirectoryHandle {
        match &self.storage {
            DirectoryStorage::Owned(handle) => handle,
            DirectoryStorage::Absolute(guard) => platform::absolute_directory_handle(guard),
        }
    }

    pub(crate) fn absolute_ancestry(&self) -> Option<&platform::AbsoluteDirectoryGuard> {
        match &self.storage {
            DirectoryStorage::Owned(_) => None,
            DirectoryStorage::Absolute(guard) => Some(guard),
        }
    }
}

pub(crate) struct DirectoryParent {
    pub(crate) directory: Directory,
    pub(crate) name: OsString,
}

#[derive(Clone)]
pub struct Directory {
    pub(crate) inner: Arc<DirectoryInner>,
}

/// An exact absolute directory admission retained without exposing its path or capability.
pub struct AdmittedAbsoluteDirectory {
    pub(crate) inner: Arc<AdmittedAbsoluteDirectoryInner>,
}

pub(crate) struct AdmittedAbsoluteDirectoryInner {
    pub(crate) directory: Directory,
}

impl_redacted_debug!(AdmittedAbsoluteDirectory);

impl AdmittedAbsoluteDirectory {
    pub fn revalidate(&self) -> io::Result<()> {
        let authority = self.inner.directory.authority()?;
        let operation = authority.enter()?;
        self.inner.directory.validate(&operation)
    }

    /// Runs `observe` in this admission's observation window; see
    /// [`Directory::observe_validated`].
    pub fn observe_validated<T>(&self, observe: impl FnOnce() -> T) -> io::Result<T> {
        self.inner.directory.observe_validated(observe)
    }

    pub fn filesystem_identity(&self) -> io::Result<DirectoryFilesystemIdentity> {
        Ok(self.inner.directory.identity()?.filesystem_identity())
    }

    pub fn acquire_root_session(&self) -> io::Result<RootSessionAcquireOutcome> {
        self.revalidate()?;
        let ancestry = self
            .inner
            .directory
            .inner
            .absolute_ancestry()
            .ok_or_else(|| io::Error::other("absolute directory admission lost its ancestry"))?;
        Ok(
            match RootSession::acquire_absolute_directory_guard(ancestry) {
                RootSessionAcquireOutcome::Acquired(mut session) => {
                    session.admission = Some(Arc::clone(&self.inner));
                    RootSessionAcquireOutcome::Acquired(session)
                }
                RootSessionAcquireOutcome::AppliedUnverified(mut obligation) => {
                    obligation.admission = Some(Arc::clone(&self.inner));
                    RootSessionAcquireOutcome::AppliedUnverified(obligation)
                }
                outcome @ RootSessionAcquireOutcome::NoEffect(_) => outcome,
            },
        )
    }
}

impl_redacted_debug!(Directory);

impl Directory {
    /// Replaces one child file through a journaled replacement record. A crash at any point
    /// leaves either the old or the new contents, and the next root acquisition completes it.
    pub fn replace_durable(
        &self,
        destination: ReplaceDestination,
        contents: &[u8],
    ) -> DurableReplaceOutcome {
        let admitted = (|| -> io::Result<_> {
            let size = u64::try_from(contents.len())
                .ok()
                .filter(|size| *size <= recovery::MAX_RECOVERABLE_FILE_BYTES)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "durable replacement exceeds the recovery bound",
                    )
                })?;
            let authority = self.authority()?;
            let operation = authority.enter()?;
            self.validate(&operation)?;
            let (parent, name, old) = admit_durable_destination(&destination, &operation)?;
            if parent.inner.identity != self.inner.identity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "durable replacement destination is not a child of this directory",
                ));
            }
            if old.is_some_and(|old| {
                old.size > recovery::MAX_RECOVERABLE_FILE_BYTES
                    || old.size + size > recovery::MAX_LIVE_PROOF_BYTES
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "durable replacement prior content exceeds the recovery bound",
                ));
            }
            Ok((name, old))
        })();
        let (name, old) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => return DurableReplaceOutcome::NoEffect(error),
        };
        let mut staged = match self.create_recoverable_stage_with_old(&name, old) {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => return DurableReplaceOutcome::NoEffect(error),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                return settle_durable_replace(DurableReplaceState::Creating {
                    cause: copy_io_error(obligation.error()),
                    obligation,
                });
            }
        };
        let prepared = staged.write_all(contents).and_then(|()| {
            let authority = self.authority()?;
            let operation = authority.enter()?;
            let (parent, current, proof) = admit_durable_destination(&destination, &operation)?;
            if parent.inner.identity != self.inner.identity || current != name || proof != old {
                return Err(identity_changed(
                    "durable replacement destination changed before commit",
                ));
            }
            Ok(())
        });
        if let Err(error) = prepared {
            return discard_durable_stage(error, staged.discard());
        }
        // Sealing journals the new proof beside the old one; from here replay rolls forward.
        let sealed = match staged.seal() {
            Ok(sealed) => sealed,
            Err(failure) => {
                let error = copy_io_error(failure.error());
                return discard_durable_stage(error, failure.into_staged().discard());
            }
        };
        drop(destination);
        match begin_live_replacement(self, sealed) {
            Ok(replay) => settle_durable_replace(DurableReplaceState::Replaying {
                parent: self.clone(),
                replay,
            }),
            Err((error, sealed)) => discard_durable_stage(error, sealed.discard()),
        }
    }

    pub fn create_recoverable_stage(&self, destination: &LeafName) -> FileCreateOutcome {
        self.create_recoverable_stage_with_old(destination, None)
    }

    pub(crate) fn create_recoverable_stage_with_old(
        &self,
        destination: &LeafName,
        old: Option<recovery::RecoveryFileProof>,
    ) -> FileCreateOutcome {
        use rand::RngCore as _;

        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        if let Err(error) = self.validate(&operation) {
            return FileCreateOutcome::NoEffect(error);
        }
        let destination = match destination
            .as_os_str()
            .to_str()
            .and_then(|name| RecoveryName::new_exact(name.to_owned()).ok())
        {
            Some(destination) => destination,
            None => {
                return FileCreateOutcome::NoEffect(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recovery destination",
                ));
            }
        };
        let parent = match recovery_parent_components(self) {
            Ok(parent) => parent,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let mut operation_id = [0; 16];
            rand::rngs::OsRng.fill_bytes(&mut operation_id);
            if operation_id == [0; 16] {
                continue;
            }
            let stage = recovery_leaf(&recovery_stage_leaf(operation_id))
                .expect("derived recovery stage is valid");
            let park = recovery_leaf(&recovery_park_leaf(operation_id))
                .expect("derived recovery park is valid");
            let record = RecoveryRecord {
                operation_id,
                phase: RecoveryPhase::StagePrepared,
                destination_parent: parent.clone(),
                destination_leaf: destination.clone(),
                old,
                new: None,
            };
            let mut state = match authority.state() {
                Ok(state) => state,
                Err(error) => return FileCreateOutcome::NoEffect(error),
            };
            if state.phase != AUTHORITY_LIVE || state.recovery.is_uncertain() {
                return FileCreateOutcome::NoEffect(stale_capability());
            }
            if state.recovery.records().next().is_some() {
                return FileCreateOutcome::NoEffect(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another recoverable publication is active",
                ));
            }
            let leaves = if old.is_some() {
                vec![(self, &stage), (self, &park)]
            } else {
                vec![(self, &stage)]
            };
            if state.namespace_footprint_is_reserved(FootprintQuery::leaves(&leaves)) {
                continue;
            }
            if let Err(error) =
                preflight_recovery_create(self, &stage, old.is_some().then_some(&park))
            {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return FileCreateOutcome::NoEffect(error);
            }
            let registration = match state.recovery.reserve(&record) {
                Ok(registration) => registration,
                Err(error) => return FileCreateOutcome::NoEffect(error),
            };
            let id = match state.allocate_id() {
                Ok(id) => id,
                Err(error) => return FileCreateOutcome::NoEffect(error),
            };
            if let Err(error) = state.reserve_effect() {
                return FileCreateOutcome::NoEffect(error);
            }
            state.stage_creations.insert(
                id,
                StageCreateRecord {
                    parent: self.clone(),
                    name: stage.clone(),
                    created: None,
                    cleanup: None,
                    identity: None,
                    phase: StageCreatePhase::Reserved,
                    checked_out: false,
                    recovery: Some(registration),
                    recovery_intent: Some(record.clone()),
                },
            );
            let mut token = StageCreateToken {
                id,
                authority: Arc::downgrade(&authority),
                armed: true,
            };
            if let Err(error) =
                state
                    .recovery
                    .create_reserved(&authority.lease, registration, record)
            {
                if state.recovery.is_uncertain() {
                    drop(state);
                    return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                        error,
                        token,
                    });
                }
                state.stage_creations.remove(&id);
                state.release_effect(&operation);
                token.armed = false;
                return FileCreateOutcome::NoEffect(error);
            }
            state
                .stage_creations
                .get_mut(&id)
                .expect("persisted recovery carrier remains registered")
                .recovery_intent = None;
            drop(state);
            return execute_stage_create(self, &stage, &authority, &operation, token);
        }
        FileCreateOutcome::NoEffect(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique recoverable stage",
        ))
    }

    pub fn validate_absolute_projection(&self, path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory projection is not absolute",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let ancestry = platform::open_absolute_directory_guard(path)?;
        if platform::absolute_directory_identity(&ancestry) != self.inner.identity.physical {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory projection does not match its retained capability",
            ));
        }
        self.validate(&operation)
    }

    /// Derive a read/process projection from retained parent bindings. Neither
    /// the supplied spelling nor the returned path grants mutation authority.
    pub fn project_from(
        &self,
        anchor: &Directory,
        anchor_path: &Path,
    ) -> io::Result<std::path::PathBuf> {
        anchor.validate_absolute_projection(anchor_path)?;
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        anchor.validate(&operation)?;
        let mut names = Vec::new();
        let mut current = self.inner.as_ref();
        while current.identity != anchor.inner.identity {
            let binding = current.parent.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory is outside projection anchor",
                )
            })?;
            names.push(binding.name.clone());
            current = binding.directory.inner.as_ref();
        }
        let mut path = anchor_path.to_path_buf();
        for name in names.into_iter().rev() {
            path.push(name);
        }
        self.validate_absolute_projection(&path)?;
        anchor.validate_absolute_projection(anchor_path)?;
        Ok(path)
    }

    pub fn create_effect_owner(&self) -> io::Result<EffectOwner> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.create_effect_owner(self.clone(), &operation)
    }

    pub(crate) fn is_within(&self, anchor: &Directory) -> bool {
        if self.inner.authority.as_ptr() != anchor.inner.authority.as_ptr() {
            return false;
        }
        let mut current = self;
        loop {
            if current.inner.identity == anchor.inner.identity {
                return true;
            }
            let Some(parent) = current.inner.parent.as_ref() else {
                return false;
            };
            current = &parent.directory;
        }
    }

    /// Validate both exact capability bindings and their retained ancestry.
    pub fn validate_within(&self, anchor: &Directory) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        anchor.validate(&operation)?;
        if !self.is_within(anchor) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory capability is outside its retained scope",
            ));
        }
        Ok(())
    }

    /// Compare retained physical ancestry across independently admitted directories.
    /// Missing ancestry or changed bindings fail closed.
    pub fn overlaps(&self, other: &Directory) -> io::Result<bool> {
        let authority = self.authority()?;
        let other_authority = other.authority()?;
        let operation = authority.enter()?;
        let other_operation = other_authority.enter()?;
        self.validate(&operation)?;
        other.validate(&other_operation)?;
        let overlaps = if directory_has_physical_ancestor(self, other.inner.identity.physical)
            || directory_has_physical_ancestor(other, self.inner.identity.physical)
        {
            true
        } else {
            let first = self.has_outer_physical_ancestor(&authority, other.inner.identity.physical);
            let second =
                other.has_outer_physical_ancestor(&other_authority, self.inner.identity.physical);
            match (first, second) {
                (Ok(true), _) | (_, Ok(true)) => true,
                (first, second) => {
                    first?;
                    second?;
                    false
                }
            }
        };
        platform::validate_root(&authority.root)?;
        platform::validate_root(&other_authority.root)?;
        self.validate(&operation)?;
        other.validate(&other_operation)?;
        Ok(overlaps)
    }

    pub(crate) fn has_outer_physical_ancestor(
        &self,
        authority: &CapabilityAuthority,
        ancestor: platform::Identity,
    ) -> io::Result<bool> {
        let mut current = self;
        loop {
            if let Some(ancestry) = current.inner.absolute_ancestry() {
                return platform::absolute_directory_anchor_is_ancestor(ancestry, ancestor);
            }
            let Some(parent) = &current.inner.parent else {
                return platform::root_has_physical_ancestor(&authority.root, ancestor);
            };
            current = &parent.directory;
        }
    }

    pub fn move_no_replace(
        self,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> DirectoryMoveOutcome {
        let Some(binding) = self.inner.parent.as_ref() else {
            return DirectoryMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a root directory capability cannot be moved",
                ),
                item: self,
            };
        };
        let source_parent = binding.directory.clone();
        let source_name = match LeafName::new(binding.name.clone()) {
            Ok(name) => name,
            Err(_) => {
                return DirectoryMoveOutcome::NoEffect {
                    error: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory binding is not a valid native leaf",
                    ),
                    item: self,
                };
            }
        };
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_rebinding() {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| destination.validate(&operation))
        {
            return DirectoryMoveOutcome::NoEffect { error, item: self };
        }
        if !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority) {
            return DirectoryMoveOutcome::NoEffect {
                error: stale_capability(),
                item: self,
            };
        }
        if source_parent.inner.identity == destination.inner.identity
            && platform::leaf_names_equal(source_name.as_os_str(), destination_name.as_os_str())
        {
            return DirectoryMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory move destination matches its source",
                ),
                item: self,
            };
        }
        let mut token = match MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: source_parent.clone(),
                name: source_name.clone(),
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            Some(self.inner.identity.physical),
            None,
        ) {
            Ok(token) => token,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect { error, item: self };
            }
        };
        let effect = platform::rename_directory_no_replace(
            source_parent.inner.handle(),
            source_name.as_os_str(),
            self.inner.handle(),
            self.inner.identity.physical,
            destination.inner.handle(),
            destination_name.as_os_str(),
        );
        let reported_success = effect.is_ok();
        match settle_directory_move(
            self,
            destination,
            destination_name,
            reported_success,
            &mut token,
        ) {
            Ok((true, directory)) => DirectoryMoveOutcome::Applied(directory),
            Ok((false, directory)) => DirectoryMoveOutcome::NoEffect {
                error: effect.err().unwrap_or_else(|| {
                    io::Error::other("directory move reported no effect after native success")
                }),
                item: directory,
            },
            Err(directory) => DirectoryMoveOutcome::AppliedUnverified(DirectoryMoveObligation {
                error: effect
                    .err()
                    .unwrap_or_else(|| io::Error::other("directory move could not be classified")),
                directory: Some(directory),
                destination: destination.clone(),
                destination_name: destination_name.clone(),
                reported_success,
                token,
            }),
        }
    }

    pub fn park(self) -> DirectoryParkOutcome {
        let mut directory = self;
        let mut last_collision = None;
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let park_name = random_leaf(".axial-dir-park-");
            match directory.park_as(park_name) {
                DirectoryParkOutcome::NoEffect {
                    error,
                    item: returned,
                } if error.kind() == io::ErrorKind::AlreadyExists => {
                    directory = returned;
                    last_collision = Some(error);
                }
                outcome => return outcome,
            }
        }
        DirectoryParkOutcome::NoEffect {
            error: last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique parked directory name",
                )
            }),
            item: directory,
        }
    }

    pub fn park_as(self, park_name: LeafName) -> DirectoryParkOutcome {
        let Some(binding) = self.inner.parent.as_ref() else {
            return DirectoryParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a root directory capability cannot be parked",
                ),
                item: self,
            };
        };
        let parent = binding.directory.clone();
        let original_name = match LeafName::new(binding.name.clone()) {
            Ok(name) => name,
            Err(_) => {
                return DirectoryParkOutcome::NoEffect {
                    error: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory binding is not a valid native leaf",
                    ),
                    item: self,
                };
            }
        };
        if platform::leaf_names_equal(original_name.as_os_str(), park_name.as_os_str()) {
            return DirectoryParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory park destination matches its source",
                ),
                item: self,
            };
        }
        let authority = match parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect { error, item: self };
            }
        };
        let operation = match authority.enter_directory_rebinding() {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect { error, item: self };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryParkOutcome::NoEffect { error, item: self };
        }
        if let Err(error) = authority.ensure_park_available(
            &operation,
            &parent,
            &original_name,
            &park_name,
            Some(self.inner.identity.physical),
        ) {
            return DirectoryParkOutcome::NoEffect { error, item: self };
        }
        let cleanup = match platform::open_parked_directory(
            parent.inner.handle(),
            original_name.as_os_str(),
            self.inner.identity.physical,
        ) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect { error, item: self };
            }
        };
        let mut token = match authority.reserve_directory_park(
            &operation,
            &parent,
            &self,
            original_name.clone(),
            park_name.clone(),
            cleanup,
        ) {
            Ok(token) => token,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect { error, item: self };
            }
        };
        let mut guard = match authority.take_directory_park(&operation, &token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                    error,
                    parent,
                    directory: Some(self),
                    original_name,
                    token,
                    park_name,
                });
            }
        };
        let effect = platform::park_directory_no_replace(
            parent.inner.handle(),
            original_name.as_os_str(),
            self.inner.handle(),
            self.inner.identity.physical,
            park_name.as_os_str(),
            &guard.record().cleanup,
        );
        match effect {
            Ok(()) => {
                if let Err(error) = parent.validate(&operation) {
                    drop(guard);
                    return DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                        error,
                        parent,
                        directory: Some(self),
                        original_name,
                        token,
                        park_name,
                    });
                }
                guard.record_mut().phase = DirectoryParkRegistryPhase::Live;
                drop(guard);
                DirectoryParkOutcome::Applied(ParkedDirectory {
                    parent,
                    original_name,
                    park_name,
                    identity: self.inner.identity,
                    token,
                    authority: self.inner.authority.clone(),
                })
            }
            Err(platform::ParkDirectoryError::NoEffect(error)) => {
                guard.disarm(&mut token, &operation);
                DirectoryParkOutcome::NoEffect { error, item: self }
            }
            Err(platform::ParkDirectoryError::AppliedUnverified(error)) => {
                drop(guard);
                DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                    error,
                    parent,
                    directory: Some(self),
                    original_name,
                    token,
                    park_name,
                })
            }
        }
    }

    pub(crate) fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        if operation.windowed
            && let Some(result) = self.validate_in_observation_window(&operation.authority)
        {
            return result;
        }
        self.validate_for_authority(&operation.authority)
    }

    pub(crate) fn validate_for_authority(
        &self,
        authority: &Arc<CapabilityAuthority>,
    ) -> io::Result<()> {
        let mut current = self.inner.as_ref();
        loop {
            validate_directory_level(current, authority)?;
            let Some(binding) = &current.parent else {
                break;
            };
            current = binding.directory.inner.as_ref();
        }
        Ok(())
    }

    /// Validates levels up to the first directory already validated in this
    /// thread's window, then caches the walked chain when it fits. Returns
    /// `None` unless this thread's window for `authority` has validated the root.
    pub(crate) fn validate_in_observation_window(
        &self,
        authority: &Arc<CapabilityAuthority>,
    ) -> Option<io::Result<()>> {
        let cached = |inner: &Arc<DirectoryInner>| {
            with_observation_window(authority, |window| {
                window
                    .root_validated
                    .then(|| window.position(inner))
                    .flatten()
            })
            .flatten()
        };
        if let Some(index) = cached(&self.inner) {
            with_observation_window(authority, |window| window.observe_cached(index));
            return Some(Ok(()));
        }
        if with_observation_window(authority, |window| window.root_validated) != Some(true) {
            return None;
        }
        let mut walked = Vec::new();
        let mut cached_ancestor = None;
        let mut current = &self.inner;
        loop {
            if let Err(error) = validate_directory_level(current, authority) {
                return Some(Err(error));
            }
            walked.push(current.clone());
            let Some(binding) = &current.parent else {
                break;
            };
            current = &binding.directory.inner;
            if let Some(index) = cached(current) {
                cached_ancestor = Some(index);
                break;
            }
        }
        let mut walked: Vec<_> = walked
            .into_iter()
            .map(|directory| ObservedDirectory {
                directory,
                observed: true,
            })
            .collect();
        let cached_walk = with_observation_window(authority, |window| {
            if !window.directories.try_extend(&mut walked) {
                return (false, window.directories.is_empty());
            }
            if let Some(index) = cached_ancestor {
                window.observe_cached(index);
            }
            (true, false)
        });
        match cached_walk {
            Some((true, _)) => return Some(Ok(())),
            // Other windows hold the budget; this observation validates fully.
            Some((false, true)) | None => return None,
            Some((false, false)) => {}
        }
        // Confirms observations so far, then keeps only the walk's cached
        // ancestors so the cache stays ancestor-closed and bounded.
        let mut retained = Vec::new();
        if cached_ancestor.is_some() {
            retained.push(current.clone());
            let mut ancestor = current.parent.as_ref();
            while let Some(binding) = ancestor {
                retained.push(binding.directory.inner.clone());
                ancestor = binding.directory.inner.parent.as_ref();
            }
        }
        if retained.len() + walked.len() > MAX_OBSERVATION_WINDOW_DIRECTORIES {
            return None;
        }
        if let Err(error) = platform::validate_lease(&authority.lease)
            .and_then(|()| platform::validate_root(&authority.root))
            .and_then(|()| confirm_observation_window(authority, true))
        {
            fail_observation_window(authority);
            return Some(Err(error));
        }
        let mut cache: Vec<_> = retained
            .into_iter()
            .map(|directory| ObservedDirectory {
                directory,
                observed: true,
            })
            .chain(walked)
            .collect();
        let cached = with_observation_window(authority, |window| {
            window.root_validated = true;
            window.observed = true;
            window.directories.try_extend(&mut cache)
        });
        // Uncached, the chain this observation relied on is validated in full.
        cached.unwrap_or(false).then_some(Ok(()))
    }
}

impl DirectoryRemovalObligation {
    pub fn reconcile(mut self) -> DirectoryRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_admitted(
        mut self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_removed_directory(
            guard.record().parent.inner.handle(),
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        ) {
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                DirectoryRemovalResolution::Applied(())
            }
            Ok(()) => {
                drop(operation);
                self.parked = Some(parked);
                DirectoryRemovalResolution::Indeterminate(self)
            }
            Err(_) => {
                drop(operation);
                self.parked = Some(parked);
                DirectoryRemovalResolution::Indeterminate(self)
            }
        }
    }

    pub(crate) fn retain(mut self, parked: ParkedDirectory) -> DirectoryRemovalResolution {
        self.parked = Some(parked);
        DirectoryRemovalResolution::Indeterminate(self)
    }
}

impl_error_accessor!(DirectoryRemovalObligation);

impl DirectoryTreeRemovalObligation {
    pub fn reconcile(mut self) -> DirectoryTreeRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("tree removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_admitted(
        self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryTreeRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let mut guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        let settled = platform::settle_removed_directory(
            guard.record().parent.inner.handle(),
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        );
        if settled.is_ok() && parked.parent.validate(&operation).is_ok() {
            guard.disarm(&mut parked.token, &operation);
            return DirectoryTreeRemovalResolution::Removed;
        }
        if settled.is_ok() {
            drop(guard);
            drop(operation);
            return self.retain(parked);
        }
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
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                DirectoryTreeRemovalResolution::Removed
            }
            _ => {
                drop(guard);
                drop(operation);
                self.retain(parked)
            }
        }
    }

    pub(crate) fn retain(mut self, parked: ParkedDirectory) -> DirectoryTreeRemovalResolution {
        self.parked = Some(parked);
        DirectoryTreeRemovalResolution::Indeterminate(self)
    }
}

impl_error_accessor!(DirectoryTreeRemovalObligation);

impl DirectoryRestoreObligation {
    pub fn reconcile(mut self) -> DirectoryRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    pub(crate) fn reconcile_admitted(
        mut self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRestoreResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_restored_directory(
            guard.record().parent.inner.handle(),
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
            guard.record().original_name.as_os_str(),
        ) {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    parked.identity,
                    parked.authority.clone(),
                    Some(DirectoryParent {
                        directory: parked.parent.clone(),
                        name: parked.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut parked.token, &operation);
                    DirectoryRestoreResolution::Applied(restored)
                } else {
                    drop(guard);
                    drop(operation);
                    self.parked = Some(parked);
                    DirectoryRestoreResolution::Indeterminate(self)
                }
            }
            Err(_) => {
                drop(guard);
                match parked.binding_state() {
                    Ok(platform::BindingState::Exact) if parked.validate(&operation).is_ok() => {
                        DirectoryRestoreResolution::NoEffect(parked)
                    }
                    _ => {
                        drop(operation);
                        self.parked = Some(parked);
                        DirectoryRestoreResolution::Indeterminate(self)
                    }
                }
            }
        }
    }

    pub(crate) fn retain(mut self, parked: ParkedDirectory) -> DirectoryRestoreResolution {
        self.parked = Some(parked);
        DirectoryRestoreResolution::Indeterminate(self)
    }
}

impl_error_accessor!(DirectoryRestoreObligation);

impl Directory {
    pub(crate) fn is_managed_root_descendant(&self) -> bool {
        let mut current = self;
        loop {
            if current.inner.absolute_ancestry().is_some() {
                return false;
            }
            match current.inner.parent.as_ref() {
                Some(parent) => current = &parent.directory,
                None => return true,
            }
        }
    }

    pub fn revision(&self) -> io::Result<DirectoryRevision> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        let stamp = platform::directory_revision(self.inner.handle())?;
        self.validate(&operation)?;
        Ok(DirectoryRevision {
            identity: self.inner.identity,
            stamp,
        })
    }

    pub fn validate_revision(&self, expected: &DirectoryRevision) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)
    }

    /// Sample retained parents in anchor-first order, excluding the descendant.
    /// Revisions are observations, not an atomic snapshot or ongoing authority.
    /// Bounds all retained parent edges at 0..=256 before allocation or I/O;
    /// separate absolute ancestry guards retain their normal validation.
    /// Returns `None` when the retained chain exceeds the requested budget.
    pub fn parent_revisions_to(
        &self,
        descendant: &Self,
        max_depth: usize,
    ) -> io::Result<Option<Vec<DirectoryRevision>>> {
        if max_depth > MAX_REVISION_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "revision depth exceeds limit",
            ));
        }
        let mut current = descendant;
        let mut depth = 0;
        let mut count = None;
        loop {
            if Arc::ptr_eq(&self.inner, &current.inner) {
                count = Some(depth);
            }
            let Some(parent) = &current.inner.parent else {
                break;
            };
            if depth == max_depth {
                return Ok(None);
            }
            depth += 1;
            current = &parent.directory;
        }
        let count = count.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "revision anchor is not retained ancestry",
            )
        })?;
        let mut revisions = Vec::new();
        revisions
            .try_reserve_exact(count)
            .map_err(io::Error::other)?;
        let authority = descendant.authority()?;
        let operation = authority.enter_observation()?;
        descendant.validate(&operation)?;
        current = descendant;
        for _ in 0..count {
            current = &current
                .inner
                .parent
                .as_ref()
                .expect("counted retained ancestry")
                .directory;
            revisions.push(DirectoryRevision {
                identity: current.inner.identity,
                stamp: platform::directory_revision(current.inner.handle())?,
            });
        }
        #[cfg(test)]
        {
            let pause = {
                let mut slot = authority
                    .revision_pause
                    .lock()
                    .map_err(|_| io::Error::other("revision pause lock poisoned"))?;
                if slot
                    .as_ref()
                    .is_some_and(|pause| pause.directory == descendant.inner.identity)
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(pause) = pause {
                pause.sampled.send(()).map_err(io::Error::other)?;
                pause.resume.recv().map_err(io::Error::other)?;
            }
        }
        authority.validate_after_observation(&operation)?;
        descendant.validate(&operation)?;
        revisions.reverse();
        Ok(Some(revisions))
    }

    pub(crate) fn validate_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &DirectoryRevision,
    ) -> io::Result<()> {
        if self.inner.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let stamp = platform::directory_revision(self.inner.handle())?;
        if expected.identity != self.inner.identity || expected.stamp != stamp {
            return Err(identity_changed("directory revision changed"));
        }
        Ok(())
    }

    /// Runs `observe` with this thread's read-only observations of this root
    /// session sharing one validation of the root, lease and each retained
    /// directory ancestry, all revalidated before a result is returned. Any
    /// non-observation entry meanwhile first revalidates the window and keeps
    /// its own full validation. Nested calls join this authority's open window.
    pub fn observe_validated<T>(&self, observe: impl FnOnce() -> T) -> io::Result<T> {
        self.validation_window(false, observe)
    }

    /// Runs `work` like [`Self::observe_validated`], except that its file
    /// effects share the window's validation too. Directory rebinding entries
    /// keep their full validation and discard the window's directories. A
    /// displacement is reported when the window closes, after effects already
    /// applied through the retained handles. Nested calls join an open window
    /// without changing its kind.
    pub fn batch_validated<T>(&self, work: impl FnOnce() -> T) -> io::Result<T> {
        self.validation_window(true, work)
    }

    pub(crate) fn validation_window<T>(
        &self,
        batch: bool,
        observe: impl FnOnce() -> T,
    ) -> io::Result<T> {
        let authority = self.authority()?;
        let key = Arc::as_ptr(&authority) as usize;
        let opened = OBSERVATION_WINDOWS.with(|windows| {
            let mut windows = windows.borrow_mut();
            if windows.len() >= MAX_OBSERVATION_WINDOW_AUTHORITIES
                || windows.iter().any(|window| window.authority == key)
            {
                return false;
            }
            windows.push(ObservationWindow {
                authority: key,
                batch,
                root_validated: false,
                observed: false,
                failed: false,
                directories: WindowDirectories::default(),
                files: Vec::new(),
                effects: 0,
            });
            true
        });
        if !opened {
            return Ok(observe());
        }
        let scope = ObservationWindowScope { authority: key };
        let value = observe();
        let closing = with_observation_window(&authority, |window| {
            let files = std::mem::take(&mut window.files);
            (
                window.failed,
                window.observed.then(|| (window.take_observed(), files)),
            )
        });
        drop(scope);
        let Some((failed, closing)) = closing else {
            return Ok(value);
        };
        if failed {
            return Err(identity_changed(
                "observation window refused a directory it relied on",
            ));
        }
        if let Some((directories, files)) = closing {
            platform::validate_lease(&authority.lease)?;
            platform::validate_root(&authority.root)?;
            validate_window_directories(&authority, &directories)?;
            validate_window_files(files.iter().filter(|file| file.observed))?;
        }
        Ok(value)
    }

    pub fn identity(&self) -> io::Result<DirectoryIdentity> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        Ok(self.inner.identity)
    }

    /// Comparison evidence from physical identity and birth time, not authority.
    /// Child edits preserve it; unavailable birth metadata refuses the proof.
    /// Filesystems may reuse or permit changes to these values.
    pub fn incarnation_witness(&self) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};

        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let created_at_ns = platform::directory_created_at_ns(self.inner.handle())?;
        let mut digest = Sha256::new();
        digest.update(b"axial-fs-directory-incarnation-v1");
        digest.update(platform::identity_witness(self.inner.identity.physical));
        digest.update(created_at_ns.to_le_bytes());
        if platform::directory_created_at_ns(self.inner.handle())? != created_at_ns {
            return Err(identity_changed("directory birth time changed"));
        }
        self.validate(&operation)?;
        Ok(digest.finalize().into())
    }

    pub fn open_directory(&self, name: &LeafName) -> io::Result<Self> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
        #[cfg(test)]
        authority.pause_directory_open_after_precheck(self, name);
        let (handle, identity) = platform::open_directory(self.inner.handle(), name.as_os_str())?;
        let opened = Self::from_handle(
            handle,
            authority.identity(identity),
            self.inner.authority.clone(),
            Some(DirectoryParent {
                directory: self.clone(),
                name: name.as_os_str().to_os_string(),
            }),
        );
        opened.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
        Ok(opened)
    }

    pub fn create_directory(&self, name: &LeafName) -> DirectoryCreateOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryCreateOutcome::NoEffect(error);
        }
        if let Err(error) =
            authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)
        {
            return DirectoryCreateOutcome::NoEffect(error);
        }
        let mut reservation = match authority.reserve_directory_create(&operation, self, name) {
            Ok(reservation) => reservation,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        let handle = match platform::create_directory(self.inner.handle(), name.as_os_str()) {
            Ok(handle) => handle,
            Err(platform::CreateDirectoryError::NoEffect(error)) => {
                match authority.take_directory_create(&operation, &reservation) {
                    Ok(guard) => guard.disarm(&mut reservation, &operation),
                    Err(settlement) => {
                        return DirectoryCreateOutcome::AppliedUnverified(
                            DirectoryCreateObligation {
                                error: io::Error::other(format!(
                                    "directory create had no native effect but its reservation did not settle: {error}; {settlement}"
                                )),
                                token: reservation,
                            },
                        );
                    }
                }
                return DirectoryCreateOutcome::NoEffect(error);
            }
            Err(platform::CreateDirectoryError::CreatedUnclassified(error)) => {
                authority.mark_directory_create_unclassified(&reservation);
                return DirectoryCreateOutcome::CreatedUnclassified {
                    error,
                    preservation: DirectoryCreatePreservation { token: reservation },
                };
            }
            #[cfg(windows)]
            Err(platform::CreateDirectoryError::AppliedUnverified { error, retained }) => {
                authority.attach_directory_create(&reservation, retained);
                return DirectoryCreateOutcome::AppliedUnverified(DirectoryCreateObligation {
                    error,
                    token: reservation,
                });
            }
        };
        authority.attach_directory_create(&reservation, handle);
        match finish_directory_create(&authority, &operation, &mut reservation) {
            Ok(directory) => DirectoryCreateOutcome::Created(directory),
            Err(error) => DirectoryCreateOutcome::AppliedUnverified(DirectoryCreateObligation {
                error,
                token: reservation,
            }),
        }
    }

    pub fn open_file(&self, name: &LeafName) -> io::Result<FileCapability> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        let file = self.open_file_unvalidated_in(name, &operation)?;
        // The open just bound the name; a window revalidates the binding later.
        if !(operation.remembers_files && file.remember_binding(&authority)?) {
            file.validate(&operation)?;
        }
        Ok(file)
    }

    pub(crate) fn open_file_unvalidated_in(
        &self,
        name: &LeafName,
        operation: &CapabilityOperation,
    ) -> io::Result<FileCapability> {
        let authority = &operation.authority;
        self.validate(operation)?;
        authority.ensure_leaf_not_root_control(operation, self, name)?;
        authority.ensure_leaf_not_directory_create_reserved(operation, self, name)?;
        authority.ensure_leaf_not_transient_reserved(operation, self, name)?;
        let handle = platform::open_file(self.inner.handle(), name.as_os_str())?;
        let identity = platform::file_identity(&handle)?;
        Ok(FileCapability::new(
            handle,
            identity,
            self.clone(),
            name.clone(),
            self.inner.authority.clone(),
        ))
    }

    /// Reobserve the exact leaf against original evidence without retaining a file.
    pub fn validate_file_revision_observation(
        &self,
        name: &LeafName,
        expected: &FileRevisionObservation,
    ) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        let file = self.open_file_unvalidated_in(name, &operation)?;
        #[cfg(test)]
        authority.pause_file_observation_after_open(self, name)?;
        // Retaining admission does not freeze externally replaceable root bindings.
        authority.validate_after_observation(&operation)?;
        file.validate_revision_observation_in(&operation, expected)
    }

    pub(crate) fn create_file_create_only(&self, name: &LeafName) -> FileCreateOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        if let Err(error) = self.validate(&operation) {
            return FileCreateOutcome::NoEffect(error);
        }
        if let Err(error) =
            authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)
        {
            return FileCreateOutcome::NoEffect(error);
        }
        let reservation = match authority.reserve_stage_create(&operation, self, name, None) {
            Ok(reservation) => reservation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        execute_stage_create(self, name, &authority, &operation, reservation)
    }

    pub fn create_stage(&self) -> FileCreateOutcome {
        use rand::RngCore;

        for _ in 0..MAX_STAGE_ATTEMPTS {
            let mut nonce = [0_u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut nonce);
            let name = format!(".axial-stage-{}", hex::encode(nonce));
            let name = LeafName::new(name).expect("generated stage leaf is valid");
            match self.create_file_create_only(&name) {
                FileCreateOutcome::NoEffect(error)
                    if error.kind() == io::ErrorKind::AlreadyExists => {}
                outcome => return outcome,
            }
        }
        FileCreateOutcome::NoEffect(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique staged file",
        ))
    }

    pub fn entries(&self, limit: usize) -> io::Result<DirectoryListing> {
        if limit == 0 || limit > MAX_DIRECTORY_LIST_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory listing limit is outside the supported range",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        let listing = platform::entries(self.inner.handle(), limit)?;
        self.validate(&operation)?;
        let state = if listing.complete {
            DirectoryListingState::Complete
        } else {
            DirectoryListingState::Truncated
        };
        let entries = listing
            .entries
            .into_iter()
            .map(|(name, kind)| DirectoryEntry {
                name,
                kind,
                parent: self.inner.identity,
            })
            .collect();
        Ok(DirectoryListing { entries, state })
    }

    /// Creates one link inside an application-owned unpublished tree.
    ///
    /// The caller must retain authority to remove the complete containing tree
    /// until that tree is published. `self` is the containing tree root,
    /// `parent` locates the destination parent beneath it, and `target` is
    /// resolved from that parent. Creation is refused unless the target stays
    /// beneath `self` and resolves to an existing non-link entry.
    #[cfg(unix)]
    pub fn create_owned_symlink_beneath(
        &self,
        parent: &[LeafName],
        name: &LeafName,
        target: &OsStr,
    ) -> io::Result<()> {
        let resolved_target = resolve_owned_symlink_target_beneath(parent, target)?;
        self.validate_owned_symlink_target(&resolved_target)?;
        let destination_parent = self.open_owned_directory_path(parent)?;
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        destination_parent.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(
            &operation,
            &destination_parent,
            name,
        )?;
        authority.ensure_leaf_not_transient_reserved(&operation, &destination_parent, name)?;
        let listing = platform::entries(
            destination_parent.inner.handle(),
            MAX_DIRECTORY_LIST_ENTRIES,
        )?;
        if !listing.complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owned symlink parent exceeds its bounded namespace",
            ));
        }
        if listing
            .entries
            .iter()
            .any(|(observed, _)| leaf_names_equivalent(observed, name.as_os_str()))
        {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        platform::create_symlink(destination_parent.inner.handle(), name.as_os_str(), target)?;
        let readback = platform::read_symlink(destination_parent.inner.handle(), name.as_os_str())?;
        if readback != target {
            return Err(identity_changed(
                "owned symlink target changed during creation",
            ));
        }
        platform::sync_directory(destination_parent.inner.handle())?;
        destination_parent.validate(&operation)?;
        self.validate(&operation)?;
        self.validate_owned_symlink_target(&resolved_target)?;
        if platform::read_symlink(destination_parent.inner.handle(), name.as_os_str())? != target {
            return Err(identity_changed(
                "owned symlink target changed after synchronization",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn open_owned_directory_path(&self, components: &[LeafName]) -> io::Result<Self> {
        let mut directory = self.clone();
        for component in components {
            directory = directory.open_directory(component)?;
        }
        Ok(directory)
    }

    #[cfg(unix)]
    pub(crate) fn validate_owned_symlink_target(&self, components: &[LeafName]) -> io::Result<()> {
        let (name, parent) = components.split_last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned symlink target must name an entry beneath its root",
            )
        })?;
        let directory = self.open_owned_directory_path(parent)?;
        let listing = directory.entries(MAX_DIRECTORY_LIST_ENTRIES)?;
        if listing.state() != DirectoryListingState::Complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owned symlink target parent exceeds its bounded namespace",
            ));
        }
        match listing
            .entries()
            .iter()
            .find(|entry| leaf_names_equivalent(entry.name(), name.as_os_str()))
        {
            Some(entry) if matches!(entry.kind(), EntryKind::File | EntryKind::Directory) => Ok(()),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned symlink target is not a file or directory",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "owned symlink target does not exist",
            )),
        }
    }

    #[cfg(unix)]
    pub fn read_symlink(&self, name: &LeafName) -> io::Result<OsString> {
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
        authority.ensure_leaf_not_transient_reserved(&operation, self, name)?;
        let first = platform::read_symlink(self.inner.handle(), name.as_os_str())?;
        self.validate(&operation)?;
        let second = platform::read_symlink(self.inner.handle(), name.as_os_str())?;
        if first != second {
            return Err(identity_changed(
                "symlink target changed during observation",
            ));
        }
        self.validate(&operation)?;
        Ok(second)
    }

    pub fn open_observed_directory(&self, entry: &DirectoryEntry) -> io::Result<Self> {
        if entry.parent != self.inner.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "observed entry belongs to another directory",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter_observation()?;
        self.validate(&operation)?;
        let name = LeafName::new(entry.name.clone()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "observed entry name is invalid")
        })?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, &name)?;
        #[cfg(test)]
        authority.pause_directory_open_after_precheck(self, &name);
        let (handle, identity) =
            platform::open_directory(self.inner.handle(), entry.name.as_os_str())?;
        let opened = Self::from_handle(
            handle,
            authority.identity(identity),
            self.inner.authority.clone(),
            Some(DirectoryParent {
                directory: self.clone(),
                name: entry.name.clone(),
            }),
        );
        opened.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, &name)?;
        Ok(opened)
    }

    pub fn admit_existing_file_park(
        &self,
        original_name: &LeafName,
        parked: FileParkRequest,
    ) -> io::Result<ParkedFile> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        parked.file.validate_bound_to(self, &operation)?;
        if platform::leaf_names_equal(original_name.as_os_str(), parked.file.name.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file park original and parked leaves must differ",
            ));
        }
        parked.validate_revision(&operation)?;
        if platform::file_binding_state(
            self.inner.handle(),
            original_name.as_os_str(),
            parked.file.identity,
        )? != platform::BindingState::Absent
            || platform::file_binding_state(
                self.inner.handle(),
                parked.file.name.as_os_str(),
                parked.file.identity,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed("existing file park topology is not exact"));
        }
        authority.ensure_park_available(
            &operation,
            self,
            original_name,
            &parked.file.name,
            None,
        )?;
        let cleanup = platform::open_parked_file(
            self.inner.handle(),
            parked.file.name.as_os_str(),
            parked.file.identity,
        )?;
        verify_parked_file(self, &parked.file.name, &cleanup, &parked.expected)?;
        self.validate(&operation)?;

        let park_name = parked.file.name.clone();
        let identity = parked.file.identity;
        let size = parked.expected.revision.size;
        let stamp = parked.expected.revision.stamp;
        let parked_authority = parked.file.authority.clone();
        let mut token = authority.register_file_park(
            &operation,
            self,
            original_name.clone(),
            park_name.clone(),
            identity,
            size,
            stamp,
            None,
            cleanup,
            FileParkRegistryPhase::Live,
        )?;

        let post_registration = (|| {
            self.validate(&operation)?;
            parked.validate_revision(&operation)?;
            if platform::file_binding_state(
                self.inner.handle(),
                original_name.as_os_str(),
                identity,
            )? != platform::BindingState::Absent
                || platform::file_binding_state(
                    self.inner.handle(),
                    park_name.as_os_str(),
                    identity,
                )? != platform::BindingState::Exact
            {
                return Err(identity_changed(
                    "existing file park topology changed after registration",
                ));
            }
            let guard = authority.take_file_park(&operation, &token)?;
            let proof =
                verify_parked_file(self, &park_name, &guard.record().cleanup, &parked.expected);
            drop(guard);
            proof?;
            parked.validate_revision(&operation)?;
            self.validate(&operation)
        })();
        if let Err(error) = post_registration {
            authority.rollback_file_park_registration(&operation, &mut token)?;
            return Err(error);
        }

        Ok(ParkedFile {
            parent: self.clone(),
            original_name: original_name.clone(),
            park_name,
            identity,
            size,
            stamp,
            verified: true,
            token,
            authority: parked_authority,
        })
    }

    pub fn admit_existing_directory_park(
        &self,
        original_name: &LeafName,
        parked: Directory,
        expected: &DirectoryRevision,
    ) -> io::Result<ParkedDirectory> {
        let authority = self.authority()?;
        let operation = authority.enter_directory_rebinding()?;
        self.validate(&operation)?;
        parked.validate(&operation)?;
        let binding = parked.inner.parent.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a root directory cannot be admitted as a park",
            )
        })?;
        binding.directory.validate(&operation)?;
        if binding.directory.inner.identity != self.inner.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parked directory belongs to another parent authority",
            ));
        }
        let park_name = LeafName::new(binding.name.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "parked directory binding is not a valid leaf",
            )
        })?;
        if platform::leaf_names_equal(original_name.as_os_str(), park_name.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory park original and parked leaves must differ",
            ));
        }
        parked.validate_revision_in(&operation, expected)?;
        if platform::directory_binding_state(
            self.inner.handle(),
            original_name.as_os_str(),
            parked.inner.identity.physical,
        )? != platform::BindingState::Absent
            || platform::directory_binding_state(
                self.inner.handle(),
                park_name.as_os_str(),
                parked.inner.identity.physical,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed(
                "existing directory park topology is not exact",
            ));
        }
        authority.ensure_park_available(
            &operation,
            self,
            original_name,
            &park_name,
            Some(parked.inner.identity.physical),
        )?;
        let cleanup = platform::open_parked_directory(
            self.inner.handle(),
            park_name.as_os_str(),
            parked.inner.identity.physical,
        )?;
        parked.validate_revision_in(&operation, expected)?;
        self.validate(&operation)?;

        let identity = parked.inner.identity;
        let parked_authority = parked.inner.authority.clone();
        let mut token = authority.register_directory_park(
            &operation,
            self,
            original_name.clone(),
            park_name.clone(),
            identity.physical,
            cleanup,
            DirectoryParkRegistryPhase::Live,
        )?;

        let post_registration = (|| {
            self.validate(&operation)?;
            if platform::directory_binding_state(
                self.inner.handle(),
                original_name.as_os_str(),
                identity.physical,
            )? != platform::BindingState::Absent
                || platform::directory_binding_state(
                    self.inner.handle(),
                    park_name.as_os_str(),
                    identity.physical,
                )? != platform::BindingState::Exact
            {
                return Err(identity_changed(
                    "existing directory park topology changed after registration",
                ));
            }
            parked.validate_revision_in(&operation, expected)?;
            parked.validate(&operation)?;
            self.validate(&operation)
        })();
        if let Err(error) = post_registration {
            authority.rollback_directory_park_registration(&operation, &mut token)?;
            return Err(error);
        }

        Ok(ParkedDirectory {
            parent: self.clone(),
            original_name: original_name.clone(),
            park_name,
            identity,
            token,
            authority: parked_authority,
        })
    }

    pub fn park_file(&self, request: FileParkRequest) -> FileParkOutcome {
        let mut request = request;
        let mut last_collision = None;
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let park_name = random_leaf(".axial-park-");
            match self.park_file_as(request, park_name) {
                FileParkOutcome::NoEffect {
                    error,
                    request: returned,
                } if error.kind() == io::ErrorKind::AlreadyExists => {
                    request = returned;
                    last_collision = Some(error);
                }
                outcome => return outcome,
            }
        }
        FileParkOutcome::NoEffect {
            error: last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique parked file name",
                )
            }),
            request,
        }
    }

    pub fn park_file_as(&self, request: FileParkRequest, park_name: LeafName) -> FileParkOutcome {
        if platform::leaf_names_equal(request.file.name.as_os_str(), park_name.as_os_str()) {
            return FileParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file park destination matches its source",
                ),
                request,
            };
        }
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        if let Err(error) = self.validate(&operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        if let Err(error) = request.file.validate_bound_to(self, &operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        if let Err(error) = request.validate_revision(&operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        if let Err(error) =
            authority.ensure_park_available(&operation, self, &request.file.name, &park_name, None)
        {
            return FileParkOutcome::NoEffect { error, request };
        }
        let cleanup = match platform::open_parked_file(
            self.inner.handle(),
            request.file.name.as_os_str(),
            request.file.identity,
        ) {
            Ok(cleanup) => cleanup,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        let mut token =
            match authority.reserve_file_park(&operation, &request, park_name.clone(), cleanup) {
                Ok(token) => token,
                Err(error) => return FileParkOutcome::NoEffect { error, request },
            };
        let guard = match authority.take_file_park(&operation, &token) {
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
        let effect = platform::park_file_no_replace(
            self.inner.handle(),
            request.file.name.as_os_str(),
            &request.file.handle,
            request.file.identity,
            park_name.as_os_str(),
            &guard.record().cleanup,
        );
        match effect {
            Ok(()) => {
                drop(guard);
                finish_new_file_park(request, park_name, token, &operation)
            }
            Err(platform::ParkFileError::NoEffect(error)) => {
                guard.disarm(&mut token, &operation);
                FileParkOutcome::NoEffect { error, request }
            }
            Err(platform::ParkFileError::AppliedUnverified(error)) => {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error,
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::Parking,
                    digest_verified: false,
                    #[cfg(test)]
                    restored_proof_pause: None,
                })
            }
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        platform::sync_directory(self.inner.handle())?;
        self.validate(&operation)
    }

    pub(crate) fn from_handle(
        handle: platform::DirectoryHandle,
        identity: DirectoryIdentity,
        authority: Weak<CapabilityAuthority>,
        parent: Option<DirectoryParent>,
    ) -> Self {
        Self {
            inner: Arc::new(DirectoryInner {
                storage: DirectoryStorage::Owned(handle),
                identity,
                authority,
                parent,
            }),
        }
    }

    pub(crate) fn from_absolute_guard(
        guard: platform::AbsoluteDirectoryGuard,
        authority: &Arc<CapabilityAuthority>,
    ) -> Self {
        let identity = authority.identity(platform::absolute_directory_identity(&guard));
        Self {
            inner: Arc::new(DirectoryInner {
                storage: DirectoryStorage::Absolute(guard),
                identity,
                authority: Arc::downgrade(authority),
                parent: None,
            }),
        }
    }

    pub(crate) fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.inner.authority.upgrade().ok_or_else(stale_capability)
    }
}
