use super::*;

fn recovery_owns_park(record: &RecoveryRecord) -> bool {
    record.old.is_some()
}

pub(crate) const AUTHORITY_LIVE: u8 = 0;

pub(crate) const AUTHORITY_QUIESCING: u8 = 1;

pub(crate) const AUTHORITY_DRAINING: u8 = 2;

pub(crate) const AUTHORITY_RESETTING: u8 = 3;

pub(crate) const AUTHORITY_REVOKED: u8 = 4;

pub(crate) const MAX_EFFECT_OWNERS: usize = 256;

pub(crate) const MAX_EFFECTS_PER_OWNER: usize = 256;

thread_local! {
    pub(crate) static TERMINAL_EFFECT_SETTLEMENT_AUTHORITY: Cell<Option<(usize, u64)>> =
        const { Cell::new(None) };
}

/// Directories retained by every thread's observation windows together, so
/// concurrent windows cannot multiply pinned handles; overflow confirms and
/// evicts the overflowing window's cache. Sized below a 128-descriptor limit
/// with room for each concurrent window's own root ancestry.
pub(crate) const MAX_OBSERVATION_WINDOW_DIRECTORIES: usize = 40;

pub(crate) static WINDOW_RETAINED_DIRECTORIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Exact file bindings an observation window remembers; overflow confirms
/// and forgets the remembered bindings.
pub(crate) const MAX_OBSERVATION_WINDOW_FILES: usize = 256;

/// Concurrently open windows per thread, one per authority. Further nested
/// observations of other authorities keep per-entry validation.
pub(crate) const MAX_OBSERVATION_WINDOW_AUTHORITIES: usize = 4;

thread_local! {
    pub(crate) static OBSERVATION_WINDOWS: RefCell<Vec<ObservationWindow>> = const { RefCell::new(Vec::new()) };
}

/// One thread's bracketed read-only observations of one authority. Every
/// directory an observation relies on, and the root and lease, are validated
/// again after that observation: when the window closes, when it overflows and
/// before any non-observation entry. A cached directory is therefore never
/// relied on for an effect or a returned result without later revalidation.
/// A batch window also admits file effects as observations; see
/// [`Directory::batch_validated`]. Exact file bindings observed in the window
/// are remembered too; they are validated again at close, and before any
/// effect entry, which then forgets them because effects may rebind files.
pub(crate) struct ObservationWindow {
    pub(crate) authority: usize,
    pub(crate) batch: bool,
    /// Lease and root were validated since the window opened or was confirmed.
    pub(crate) root_validated: bool,
    /// An observation relied on the window since it opened or was confirmed.
    pub(crate) observed: bool,
    /// A confirmation refused; the window closes with an error.
    pub(crate) failed: bool,
    /// Ancestor-closed: each entry's parent is cached too, or it has none.
    pub(crate) directories: WindowDirectories,
    pub(crate) files: Vec<ObservedFile>,
    /// Live effect operations, during which no file binding is remembered.
    pub(crate) effects: usize,
}

pub(crate) struct ObservedFile {
    pub(crate) parent: Arc<DirectoryInner>,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    /// Relied on since it was last validated.
    pub(crate) observed: bool,
}

pub(crate) struct ObservedDirectory {
    pub(crate) directory: Arc<DirectoryInner>,
    /// Relied on since the last confirmation, so it must be validated again.
    pub(crate) observed: bool,
}

/// A window's cached directories, charged to the process-wide budget.
#[derive(Default)]
pub(crate) struct WindowDirectories(pub(crate) Vec<ObservedDirectory>);

impl WindowDirectories {
    /// Caches every directory, or none when the budget lacks room for all.
    pub(crate) fn try_extend(&mut self, directories: &mut Vec<ObservedDirectory>) -> bool {
        let count = directories.len();
        let reserved = WINDOW_RETAINED_DIRECTORIES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |retained| {
                retained
                    .checked_add(count)
                    .filter(|total| *total <= MAX_OBSERVATION_WINDOW_DIRECTORIES)
            })
            .is_ok();
        if reserved {
            self.0.append(directories);
        }
        reserved
    }
}

impl std::ops::Deref for WindowDirectories {
    type Target = [ObservedDirectory];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for WindowDirectories {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for WindowDirectories {
    fn drop(&mut self) {
        WINDOW_RETAINED_DIRECTORIES.fetch_sub(self.0.len(), Ordering::AcqRel);
    }
}

impl ObservationWindow {
    pub(crate) fn position(&self, directory: &Arc<DirectoryInner>) -> Option<usize> {
        self.directories
            .iter()
            .position(|cached| Arc::ptr_eq(&cached.directory, directory))
    }

    /// Marks a cached directory and its cached ancestors as relied on.
    pub(crate) fn observe_cached(&mut self, mut index: usize) {
        loop {
            let cached = &mut self.directories[index];
            if cached.observed {
                return;
            }
            cached.observed = true;
            let Some(binding) = &cached.directory.parent else {
                return;
            };
            let parent = binding.directory.inner.clone();
            match self.position(&parent) {
                Some(next) => index = next,
                None => return,
            }
        }
    }

    /// Takes the directories that need validation and clears their marks.
    pub(crate) fn take_observed(&mut self) -> Vec<Arc<DirectoryInner>> {
        self.observed = false;
        self.directories
            .iter_mut()
            .filter_map(|cached| {
                std::mem::take(&mut cached.observed).then(|| cached.directory.clone())
            })
            .collect()
    }
}

pub(crate) struct ObservationWindowScope {
    pub(crate) authority: usize,
}

impl Drop for ObservationWindowScope {
    fn drop(&mut self) {
        let window = OBSERVATION_WINDOWS.with(|windows| {
            let mut windows = windows.borrow_mut();
            windows
                .iter()
                .position(|window| window.authority == self.authority)
                .map(|index| windows.swap_remove(index))
        });
        drop(window);
    }
}

pub(crate) fn with_observation_window<T>(
    authority: &Arc<CapabilityAuthority>,
    use_window: impl FnOnce(&mut ObservationWindow) -> T,
) -> Option<T> {
    let key = Arc::as_ptr(authority) as usize;
    OBSERVATION_WINDOWS.with(|windows| {
        windows
            .borrow_mut()
            .iter_mut()
            .find(|window| window.authority == key)
            .map(use_window)
    })
}

/// Validates the directories observed since the window's last confirmation.
/// The caller validates the lease and root itself. Discarding also releases
/// every cached directory handle.
pub(crate) fn confirm_observation_window(
    authority: &Arc<CapabilityAuthority>,
    discard: bool,
) -> io::Result<()> {
    let Some((observed, discarded, files)) = with_observation_window(authority, |window| {
        let observed = window.take_observed();
        window.root_validated = !discard;
        // Discarding the directory cache also forgets the file bindings under it.
        let (discarded, files) = if discard {
            (
                std::mem::take(&mut window.directories),
                std::mem::take(&mut window.files),
            )
        } else {
            (WindowDirectories::default(), Vec::new())
        };
        (observed, discarded, files)
    }) else {
        return Ok(());
    };
    drop(discarded);
    validate_window_directories(authority, &observed)
        .and_then(|()| validate_window_files(files.iter().filter(|file| file.observed)))
        .inspect_err(|_| fail_observation_window(authority))
}

/// Records a refused confirmation so the window cannot close as validated.
pub(crate) fn fail_observation_window(authority: &Arc<CapabilityAuthority>) {
    with_observation_window(authority, |window| {
        window.failed = true;
        window.root_validated = false;
    });
}

/// Counts a non-observation operation in this thread's window, if any.
pub(crate) fn note_window_effect(authority: &Arc<CapabilityAuthority>) -> bool {
    with_observation_window(authority, |window| window.effects += 1).is_some()
}

/// Validates the remembered file bindings the window relied on, then forgets
/// them all before an effect that may rebind them.
pub(crate) fn forget_window_files(authority: &Arc<CapabilityAuthority>) -> io::Result<()> {
    let Some(files) =
        with_observation_window(authority, |window| std::mem::take(&mut window.files))
    else {
        return Ok(());
    };
    validate_window_files(files.iter().filter(|file| file.observed))
}

/// Like [`forget_window_files`] for cleanup that must proceed regardless; a
/// refusal fails the window instead.
pub(crate) fn forget_window_files_or_fail(authority: &Arc<CapabilityAuthority>) {
    if forget_window_files(authority).is_err() {
        fail_observation_window(authority);
    }
}

pub(crate) fn validate_window_files<'a>(
    files: impl Iterator<Item = &'a ObservedFile>,
) -> io::Result<()> {
    for file in files {
        if !platform::exact_file_binding(
            file.parent.handle(),
            file.name.as_os_str(),
            file.identity,
        )? {
            return Err(identity_changed("file capability changed binding"));
        }
    }
    Ok(())
}

pub(crate) fn validate_window_directories(
    authority: &Arc<CapabilityAuthority>,
    directories: &[Arc<DirectoryInner>],
) -> io::Result<()> {
    for directory in directories {
        validate_directory_level(directory, authority)?;
    }
    Ok(())
}

pub(crate) fn validate_directory_level(
    directory: &DirectoryInner,
    authority: &Arc<CapabilityAuthority>,
) -> io::Result<()> {
    if directory.authority.as_ptr() != Arc::as_ptr(authority) {
        return Err(stale_capability());
    }
    let identity = match &directory.storage {
        DirectoryStorage::Owned(handle) => platform::directory_identity(handle)?,
        DirectoryStorage::Absolute(guard) => platform::validate_absolute_directory_guard(guard)?,
    };
    if identity != directory.identity.physical {
        return Err(identity_changed("directory capability changed identity"));
    }
    if let Some(binding) = &directory.parent
        && platform::directory_binding_state(
            binding.directory.inner.handle(),
            &binding.name,
            directory.identity.physical,
        )? != platform::BindingState::Exact
    {
        return Err(identity_changed("directory capability changed binding"));
    }
    Ok(())
}

pub(crate) struct TerminalEffectSettlementScope {
    pub(crate) previous: Option<(usize, u64)>,
}

impl TerminalEffectSettlementScope {
    pub(crate) fn begin(authority: &Arc<CapabilityAuthority>, owner_id: u64) -> io::Result<Self> {
        let current = (Arc::as_ptr(authority) as usize, owner_id);
        TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
            let previous = slot.get();
            if previous.is_some() && previous != Some(current) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a different filesystem effect owner is already settling on this thread",
                ));
            }
            slot.set(Some(current));
            Ok(Self { previous })
        })
    }
}

impl Drop for TerminalEffectSettlementScope {
    fn drop(&mut self) {
        TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| slot.set(self.previous));
    }
}

pub(crate) fn terminal_effect_settlement_admits(authority: &CapabilityAuthority) -> bool {
    let authority = authority as *const CapabilityAuthority as usize;
    TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
        slot.get()
            .is_some_and(|(settling_authority, _)| settling_authority == authority)
    })
}

pub(crate) fn terminal_effect_settlement_admits_owner(
    authority: &CapabilityAuthority,
    owner_id: u64,
) -> bool {
    TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
        slot.get() == Some((authority as *const CapabilityAuthority as usize, owner_id))
    })
}

pub(crate) struct TerminalQuiescingRollback<'a> {
    pub(crate) authority: &'a CapabilityAuthority,
    pub(crate) armed: bool,
}

impl<'a> TerminalQuiescingRollback<'a> {
    pub(crate) fn new(authority: &'a CapabilityAuthority) -> Self {
        Self {
            authority,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalQuiescingRollback<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.authority.restore_live_after_quiescing();
        }
    }
}

pub(crate) fn validate_terminal_registry_state(state: &OperationState) -> io::Result<()> {
    let registered_effects = state
        .stages
        .len()
        .checked_add(state.stage_creations.len())
        .and_then(|count| count.checked_add(state.directory_creations.len()))
        .and_then(|count| count.checked_add(state.file_parks.len()))
        .and_then(|count| count.checked_add(state.directory_parks.len()))
        .and_then(|count| count.checked_add(state.moves.len()))
        .and_then(|count| count.checked_add(state.transients.len()))
        .ok_or_else(|| io::Error::other("filesystem effect registry count overflowed"))?;
    if state.outstanding_effects != registered_effects {
        return Err(io::Error::other(
            "filesystem effect registry accounting is inconsistent",
        ));
    }
    if !state.moves.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an unsettled move obligation",
        ));
    }
    if state
        .file_parks
        .values()
        .any(|record| record.phase != FileParkRegistryPhase::Abandoned)
        || state
            .directory_parks
            .values()
            .any(|record| record.phase != DirectoryParkRegistryPhase::Abandoned)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an externally owned park obligation",
        ));
    }
    if state.stages.iter().any(|(id, record)| {
        record.carrier != StageCarrierState::Abandoned
            && !transient::stage_retained_for_drain(state, *id)
    }) || state.stage_creations.values().any(|record| {
        !matches!(
            record.phase,
            StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
        )
    }) || state.directory_creations.values().any(|record| {
        !matches!(
            record.phase,
            DirectoryCreateEffectPhase::Abandoned
                | DirectoryCreateEffectPhase::CleanupAttempted
                | DirectoryCreateEffectPhase::UnclassifiedAbandoned
        )
    }) || state
        .transients
        .values()
        .any(|record| record.phase != transient::TransientEffectPhase::Abandoned)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an externally owned effect obligation",
        ));
    }
    Ok(())
}

pub(crate) struct CapabilityAuthority {
    pub(crate) operations: Mutex<OperationState>,
    #[cfg(test)]
    pub(crate) directory_open_pause: Mutex<Option<DirectoryOpenReservationPause>>,
    #[cfg(test)]
    pub(crate) file_observation_pause: Mutex<Option<FileObservationPause>>,
    #[cfg(test)]
    pub(crate) revision_pause: Mutex<Option<RevisionPause>>,
    pub(crate) session_nonce: [u8; 16],
    pub(crate) root: platform::RootGuard,
    pub(crate) lease: platform::LeaseHandle,
    pub(crate) process_image: platform::ProcessImageAncestry,
}

#[cfg(test)]
pub(crate) struct DirectoryOpenReservationPause {
    pub(crate) parent: DirectoryIdentity,
    pub(crate) name: LeafName,
    pub(crate) prechecked: Arc<std::sync::Barrier>,
    pub(crate) resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
pub(crate) struct FileObservationPause {
    pub(crate) parent: DirectoryIdentity,
    pub(crate) name: LeafName,
    pub(crate) opened: std::sync::mpsc::SyncSender<()>,
    pub(crate) resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct RevisionPause {
    pub(crate) directory: DirectoryIdentity,
    pub(crate) sampled: std::sync::mpsc::SyncSender<()>,
    pub(crate) resume: std::sync::mpsc::Receiver<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StageRegistryPhase {
    Writing,
    Sealed,
    CleanupAttempted,
    PromotionAttempted,
    Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StageCarrierState {
    Live,
    Abandoned,
}

pub(crate) struct StageRecord {
    pub(crate) parent: Directory,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) cleanup: Option<platform::FileCleanupHandle>,
    pub(crate) phase: StageRegistryPhase,
    pub(crate) carrier: StageCarrierState,
    pub(crate) promotion: Option<StagePromotionRecord>,
    pub(crate) recovery: Option<recovery::RecoveryRegistration>,
}

pub(crate) struct StagePromotionRecord {
    pub(crate) destination: NamespaceLeaf,
    pub(crate) attempt_id: u64,
    pub(crate) receipt: platform::PublicationReceipt,
    pub(crate) displaced_park: Option<u64>,
}

pub(crate) struct NamespaceLeaf {
    pub(crate) parent: Directory,
    pub(crate) name: LeafName,
}

pub(crate) struct RecoveryOrphan {
    pub(crate) registration: recovery::RecoveryRegistration,
    pub(crate) ancestors: Vec<platform::Identity>,
    pub(crate) files: Vec<platform::Identity>,
}

pub(crate) struct MoveEffectRecord {
    pub(crate) source: NamespaceLeaf,
    pub(crate) destination: NamespaceLeaf,
    pub(crate) moved_directory: Option<platform::Identity>,
    pub(crate) moved_file: Option<platform::Identity>,
}

pub(crate) fn directory_has_physical_ancestor(
    directory: &Directory,
    ancestor: platform::Identity,
) -> bool {
    let mut current = directory;
    loop {
        if current.inner.identity.physical == ancestor {
            return true;
        }
        if current
            .inner
            .absolute_ancestry()
            .is_some_and(|guard| platform::absolute_directory_has_ancestor(guard, ancestor))
        {
            return true;
        }
        let Some(parent) = current.inner.parent.as_ref() else {
            return false;
        };
        current = &parent.directory;
    }
}

pub(crate) struct StageToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StageCreatePhase {
    Reserved,
    Applied,
    Abandoned,
    CleanupAttempted,
}

pub(crate) struct StageCreateRecord {
    pub(crate) parent: Directory,
    pub(crate) name: LeafName,
    pub(crate) created: Option<File>,
    pub(crate) cleanup: Option<platform::FileCleanupHandle>,
    pub(crate) identity: Option<platform::Identity>,
    pub(crate) phase: StageCreatePhase,
    pub(crate) checked_out: bool,
    pub(crate) recovery: Option<recovery::RecoveryRegistration>,
    pub(crate) recovery_intent: Option<recovery::RecoveryRecord>,
}

pub(crate) struct StageCreateToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

impl_redacted_debug!(StageCreateToken);

pub(crate) struct StageCreateRecordGuard {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) id: u64,
    pub(crate) record: Option<StageCreateRecord>,
}

impl StageCreateRecordGuard {
    pub(crate) fn record(&self) -> &StageCreateRecord {
        self.record
            .as_ref()
            .expect("stage create guard retains record")
    }

    pub(crate) fn record_mut(&mut self) -> &mut StageCreateRecord {
        self.record
            .as_mut()
            .expect("stage create guard retains record")
    }

    pub(crate) fn disarm(
        mut self,
        token: &mut StageCreateToken,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        let recovery = self.record().recovery;
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(registration) = recovery {
            clear_recovery(&self.authority.lease, &mut state, registration)?;
        }
        self.record
            .take()
            .expect("stage create guard retains record");
        let removed = state
            .stage_creations
            .remove(&self.id)
            .expect("checked-out stage create header remains registered");
        assert!(removed.checked_out);
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    pub(crate) fn finish_transfer(mut self, token: &StageCreateToken) {
        self.record
            .take()
            .expect("stage create guard retains record");
        assert!(!token.armed);
    }
}

impl Drop for StageCreateRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .stage_creations
            .get_mut(&self.id)
            .expect("checked-out stage create header remains registered");
        assert!(header.checked_out);
        header.created = record.created;
        header.cleanup = record.cleanup;
        header.identity = record.identity;
        header.phase = record.phase;
        header.checked_out = false;
        header.recovery = record.recovery;
        header.recovery_intent = record.recovery_intent;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryCreateEffectPhase {
    Reserved,
    Applied,
    Abandoned,
    CleanupAttempted,
    CreatedUnclassified,
    UnclassifiedAbandoned,
    UnclassifiedRecovery,
    CreatedUnclassifiedResetPending,
}

pub(crate) struct DirectoryCreateEffectRecord {
    pub(crate) parent: Directory,
    pub(crate) name: LeafName,
    pub(crate) created: Option<platform::DirectoryHandle>,
    pub(crate) cleanup: Option<platform::DirectoryCleanupHandle>,
    pub(crate) identity: Option<platform::Identity>,
    pub(crate) phase: DirectoryCreateEffectPhase,
    pub(crate) checked_out: bool,
}

pub(crate) struct DirectoryCreateEffectToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

impl_redacted_debug!(DirectoryCreateEffectToken);

pub(crate) struct MoveEffectToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

impl_redacted_debug!(MoveEffectToken);

impl MoveEffectToken {
    pub(crate) fn reserve(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        source: NamespaceLeaf,
        destination: NamespaceLeaf,
        moved_directory: Option<platform::Identity>,
        moved_file: Option<platform::Identity>,
    ) -> io::Result<Self> {
        if !Arc::ptr_eq(authority, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = authority.state()?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        let record = MoveEffectRecord {
            source,
            destination,
            moved_directory,
            moved_file,
        };
        if state.namespace_footprint_is_reserved(FootprintQuery {
            leaves: &[
                (&record.source.parent, &record.source.name),
                (&record.destination.parent, &record.destination.name),
            ],
            subtree: record
                .moved_directory
                .map(|identity| (identity, &record.source.parent)),
            file: moved_file,
            ..FootprintQuery::default()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem move conflicts with an unsettled namespace effect",
            ));
        }
        let id = state.reserve_move_effect(record)?;
        Ok(Self {
            id,
            authority: Arc::downgrade(authority),
            armed: true,
        })
    }

    pub(crate) fn settle(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        if !Arc::ptr_eq(&authority, &operation.authority) {
            return Err(stale_capability());
        }
        authority.release_move_effect(self.id, operation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for MoveEffectToken {
    fn drop(&mut self) {
        if self.armed {
            std::process::abort();
        }
    }
}

pub(crate) struct DirectoryCreateEffectGuard {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) id: u64,
    pub(crate) record: Option<DirectoryCreateEffectRecord>,
}

impl DirectoryCreateEffectGuard {
    pub(crate) fn record(&self) -> &DirectoryCreateEffectRecord {
        self.record
            .as_ref()
            .expect("directory create guard retains record")
    }

    pub(crate) fn record_mut(&mut self) -> &mut DirectoryCreateEffectRecord {
        self.record
            .as_mut()
            .expect("directory create guard retains record")
    }

    pub(crate) fn disarm(
        mut self,
        token: &mut DirectoryCreateEffectToken,
        operation: &CapabilityOperation,
    ) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record
            .take()
            .expect("directory create guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .directory_creations
            .remove(&self.id)
            .expect("checked-out directory create header remains registered");
        assert!(removed.checked_out);
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for DirectoryCreateEffectGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .directory_creations
            .get_mut(&self.id)
            .expect("checked-out directory create header remains registered");
        assert!(header.checked_out);
        header.created = record.created;
        header.cleanup = record.cleanup;
        header.identity = record.identity;
        header.phase = record.phase;
        header.checked_out = false;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileParkRegistryPhase {
    Reserved,
    Live,
    Abandoned,
}

pub(crate) struct FileParkRegistryRecord {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) stamp: platform::FileStamp,
    pub(crate) expected_digest: Option<[u8; 32]>,
    pub(crate) cleanup: Option<platform::FileCleanupHandle>,
    pub(crate) phase: FileParkRegistryPhase,
    pub(crate) linked_effect: Option<FileParkLink>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileParkLink {
    Stage(u64),
}

pub(crate) struct FileParkSettlementRecord {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) size: u64,
    pub(crate) stamp: platform::FileStamp,
    pub(crate) expected_digest: Option<[u8; 32]>,
    pub(crate) cleanup: platform::FileCleanupHandle,
    pub(crate) phase: FileParkRegistryPhase,
}

pub(crate) struct FileParkRegistryToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryParkRegistryPhase {
    Reserved,
    Live,
    Abandoned,
}

pub(crate) struct DirectoryParkRegistryRecord {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) cleanup: Option<platform::DirectoryCleanupHandle>,
    pub(crate) phase: DirectoryParkRegistryPhase,
}

pub(crate) struct DirectoryParkSettlementRecord {
    pub(crate) parent: Directory,
    pub(crate) original_name: LeafName,
    pub(crate) name: LeafName,
    pub(crate) identity: platform::Identity,
    pub(crate) cleanup: platform::DirectoryCleanupHandle,
    pub(crate) phase: DirectoryParkRegistryPhase,
}

pub(crate) struct DirectoryParkRegistryToken {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DrainRecoveryParkId {
    File(u64),
    Directory(u64),
    DirectoryCreate(u64),
}

pub(crate) struct DrainRecoveryPermit {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) park_token_id: DrainRecoveryParkId,
}

impl_redacted_debug!(DrainRecoveryPermit);

impl_redacted_debug!(FileParkRegistryToken);

impl_redacted_debug!(DirectoryParkRegistryToken);

pub(crate) struct FileParkRecordGuard {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) id: u64,
    pub(crate) record: Option<FileParkSettlementRecord>,
}

impl FileParkRecordGuard {
    pub(crate) fn record(&self) -> &FileParkSettlementRecord {
        self.record
            .as_ref()
            .expect("file park guard retains record")
    }

    pub(crate) fn record_mut(&mut self) -> &mut FileParkSettlementRecord {
        self.record
            .as_mut()
            .expect("file park guard retains record")
    }

    pub(crate) fn disarm(
        mut self,
        token: &mut FileParkRegistryToken,
        operation: &CapabilityOperation,
    ) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record.take().expect("file park guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .file_parks
            .remove(&self.id)
            .expect("checked-out file park header remains registered");
        assert!(removed.cleanup.is_none());
        assert!(removed.linked_effect.is_none());
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for FileParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .file_parks
            .get_mut(&self.id)
            .expect("checked-out file park header remains registered");
        assert!(header.cleanup.is_none());
        header.size = record.size;
        header.stamp = record.stamp;
        header.expected_digest = record.expected_digest;
        header.cleanup = Some(record.cleanup);
        header.phase = record.phase;
    }
}

pub(crate) struct DirectoryParkRecordGuard {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) id: u64,
    pub(crate) record: Option<DirectoryParkSettlementRecord>,
}

pub(crate) enum SessionDrainSettlement {
    Ready,
    Pending,
    Recovery {
        recovery: SessionDrainRecoveryState,
        permits: Vec<DrainRecoveryPermit>,
    },
}

impl DirectoryParkRecordGuard {
    pub(crate) fn record(&self) -> &DirectoryParkSettlementRecord {
        self.record
            .as_ref()
            .expect("directory park guard retains record")
    }

    pub(crate) fn record_mut(&mut self) -> &mut DirectoryParkSettlementRecord {
        self.record
            .as_mut()
            .expect("directory park guard retains record")
    }

    pub(crate) fn disarm(
        mut self,
        token: &mut DirectoryParkRegistryToken,
        operation: &CapabilityOperation,
    ) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record
            .take()
            .expect("directory park guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .directory_parks
            .remove(&self.id)
            .expect("checked-out directory park header remains registered");
        assert!(removed.cleanup.is_none());
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for DirectoryParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .directory_parks
            .get_mut(&self.id)
            .expect("checked-out directory park header remains registered");
        assert!(header.cleanup.is_none());
        header.cleanup = Some(record.cleanup);
        header.phase = record.phase;
    }
}

impl_redacted_debug!(StageToken);

impl CapabilityAuthority {
    pub(crate) fn state(&self) -> io::Result<std::sync::MutexGuard<'_, OperationState>> {
        self.operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))
    }

    pub(crate) fn validate_retained_process_image_outside_root(&self) -> io::Result<()> {
        platform::validate_process_image_outside_root(&self.process_image, &self.root)
    }

    pub(crate) fn release_move_effect(
        self: &Arc<Self>,
        id: u64,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if (state.phase != AUTHORITY_LIVE
            && !(state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self)))
            || state.active == 0
        {
            return Err(stale_capability());
        }
        state.moves.remove(&id).ok_or_else(stale_capability)?;
        state.release_effect(operation);
        Ok(())
    }

    pub(crate) fn enter(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        if with_observation_window(self, |window| window.batch) == Some(true) {
            forget_window_files(self).inspect_err(|_| fail_observation_window(self))?;
            let mut operation = self.enter_windowed(false)?;
            operation.window_effect = note_window_effect(self);
            return Ok(operation);
        }
        let mut operation = self.admit_operation()?;
        operation.window_effect = note_window_effect(self);
        self.validate_entry(false)?;
        Ok(operation)
    }

    /// Enters an effect that may move, park, restore or remove a directory.
    pub(crate) fn enter_directory_rebinding(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        let mut operation = self.admit_operation()?;
        operation.window_effect = note_window_effect(self);
        self.validate_entry(true)?;
        Ok(operation)
    }

    /// Enters a read-only observation. Inside this thread's observation window
    /// for this authority, the window's bracketing root and lease validation
    /// replaces the per-entry validation.
    pub(crate) fn enter_observation(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        self.enter_windowed(true)
    }

    /// Enters within this thread's window, if any; only an `observation` may
    /// remember the exact file bindings it validates.
    pub(crate) fn enter_windowed(
        self: &Arc<Self>,
        observation: bool,
    ) -> io::Result<CapabilityOperation> {
        let Some(root_validated) = with_observation_window(self, |window| window.root_validated)
        else {
            return self.enter();
        };
        let mut operation = self.admit_operation()?;
        if !root_validated {
            platform::validate_lease(&self.lease)?;
            platform::validate_root(&self.root)?;
        }
        with_observation_window(self, |window| {
            window.root_validated = true;
            window.observed = true;
        });
        operation.windowed = true;
        operation.remembers_files = observation;
        Ok(operation)
    }

    /// Revalidates the lease and root after a sample; a window defers this to
    /// its closing validation, which follows every observation it admitted.
    pub(crate) fn validate_after_observation(
        &self,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if operation.windowed {
            return Ok(());
        }
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)
    }

    pub(crate) fn admit_operation(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        if state.phase != AUTHORITY_LIVE
            && !(matches!(state.phase, AUTHORITY_QUIESCING | AUTHORITY_DRAINING)
                && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        if state.recovery.is_uncertain() {
            state.recovery.reconcile_uncertain(&self.lease)?;
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        Ok(CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: false,
        })
    }

    /// Validates the root and lease for a non-observation entry and confirms
    /// this thread's observation window before any effect relies on it. File
    /// effects keep the confirmed directories; a directory rebinding entry
    /// discards them so the window retains no handle to a moved or removed
    /// directory.
    pub(crate) fn validate_entry(self: &Arc<Self>, rebinding: bool) -> io::Result<()> {
        platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| forget_window_files(self))
            .and_then(|()| confirm_observation_window(self, rebinding))
            .inspect_err(|_| fail_observation_window(self))
    }

    pub(crate) fn enter_effect_settlement(
        self: &Arc<Self>,
        owner_id: u64,
        terminal: bool,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let expected_phase = if terminal {
            AUTHORITY_QUIESCING
        } else {
            AUTHORITY_LIVE
        };
        if state.phase != expected_phase
            || (terminal && !terminal_effect_settlement_admits_owner(self, owner_id))
            || state
                .effect_owner_handles
                .get(&owner_id)
                .is_none_or(|owner| owner.strong_count() == 0)
        {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(true)?;
        Ok(operation)
    }

    pub(crate) fn enter_effect_retention(
        self: &Arc<Self>,
        owner_id: u64,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        if state.phase != AUTHORITY_LIVE || !state.effect_owner_handles.contains_key(&owner_id) {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(true)?;
        Ok(operation)
    }

    pub(crate) fn create_effect_owner(
        self: &Arc<Self>,
        anchor: Directory,
        operation: &CapabilityOperation,
    ) -> io::Result<EffectOwner> {
        if !Arc::ptr_eq(self, &operation.authority)
            || anchor.inner.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        anchor.validate(operation)?;
        let mut state = self.state()?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        state
            .effect_owner_handles
            .retain(|_, owner| owner.strong_count() > 0);
        if state.effect_owner_handles.len() >= MAX_EFFECT_OWNERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect-owner capacity is exhausted",
            ));
        }
        let id = state.allocate_id()?;
        let owner = Arc::new(EffectOwnerState {
            id,
            authority: Arc::downgrade(self),
            anchor,
            effects: Mutex::new(EffectOwnerRecords {
                next_id: 1,
                settling: false,
                in_flight: 0,
                effects: BTreeMap::new(),
            }),
            #[cfg(test)]
            settlement_pause: Mutex::new(None),
        });
        state
            .effect_owner_handles
            .insert(id, Arc::downgrade(&owner));
        Ok(EffectOwner { state: owner })
    }

    pub(crate) fn retain_effect_owner_record<T>(
        self: &Arc<Self>,
        owner: &Arc<EffectOwnerState>,
        operation: &CapabilityOperation,
        carrier: T,
        wrap: impl FnOnce(T) -> OwnedEffect,
    ) -> Result<u64, EffectOwnerRetentionError<T>> {
        if !Arc::ptr_eq(self, &operation.authority) || owner.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        }
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase != AUTHORITY_LIVE
            || !state
                .effect_owner_handles
                .get(&owner.id)
                .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(owner)))
        {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        }
        let mut records = owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = records.effects.len().saturating_add(records.in_flight);
        if retained >= MAX_EFFECTS_PER_OWNER {
            return Err(EffectOwnerRetentionError::new(
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect-owner capacity is exhausted",
                ),
                carrier,
            ));
        }
        let id = records.next_id;
        let Some(next_id) = records.next_id.checked_add(1) else {
            return Err(EffectOwnerRetentionError::new(
                io::Error::other("filesystem owned-effect id overflowed"),
                carrier,
            ));
        };
        records.next_id = next_id;
        assert!(records.effects.insert(id, wrap(carrier)).is_none());
        state
            .active_effect_owners
            .entry(owner.id)
            .or_insert_with(|| owner.clone());
        Ok(id)
    }

    pub(crate) fn deactivate_effect_owner_if_empty(&self, owner: &Arc<EffectOwnerState>) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let records = owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = !records.settling && records.in_flight == 0 && records.effects.is_empty();
        if empty {
            state.active_effect_owners.remove(&owner.id);
        }
    }

    pub(crate) fn stage_create_is_within(
        &self,
        token: &StageCreateToken,
        anchor: &Directory,
    ) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .stage_creations
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    pub(crate) fn directory_create_is_within(
        &self,
        token: &DirectoryCreateEffectToken,
        anchor: &Directory,
    ) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .directory_creations
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    pub(crate) fn stage_is_within(&self, token: &StageToken, anchor: &Directory) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .stages
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    pub(crate) fn enter_file_park(
        self: &Arc<Self>,
        token: &FileParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let admitted = token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && state
                .file_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == FileParkRegistryPhase::Live);
        let phase_admitted = state.phase == AUTHORITY_LIVE
            || (state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self));
        if !admitted || !phase_admitted {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(false)?;
        Ok(operation)
    }

    pub(crate) fn enter_file_park_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &FileParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::File(token.id)
            && state
                .file_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == FileParkRegistryPhase::Live);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(false)?;
        Ok(operation)
    }

    pub(crate) fn enter_directory_park(
        self: &Arc<Self>,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let admitted = token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && state
                .directory_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == DirectoryParkRegistryPhase::Live);
        let phase_admitted = state.phase == AUTHORITY_LIVE
            || (state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self));
        if !admitted || !phase_admitted {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(true)?;
        Ok(operation)
    }

    pub(crate) fn enter_directory_park_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::Directory(token.id)
            && state
                .directory_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == DirectoryParkRegistryPhase::Live);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(true)?;
        Ok(operation)
    }

    pub(crate) fn enter_directory_create_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &DirectoryCreateEffectToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::DirectoryCreate(token.id)
            && state
                .directory_creations
                .get(&token.id)
                .is_some_and(|record| {
                    record.phase == DirectoryCreateEffectPhase::UnclassifiedRecovery
                });
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        self.validate_entry(true)?;
        Ok(operation)
    }

    pub(crate) fn transfer_unclassified_directory_create_to_reset(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &mut DirectoryCreateEffectToken,
    ) -> io::Result<()> {
        let mut state = self.state()?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::DirectoryCreate(token.id);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .filter(|record| record.phase == DirectoryCreateEffectPhase::UnclassifiedRecovery)
            .ok_or_else(stale_capability)?;
        if !record.parent.is_managed_root_descendant() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external directory creation cannot transfer to app-root reset",
            ));
        }
        record.phase = DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending;
        token.armed = false;
        Ok(())
    }

    pub(crate) fn enter_reset_operation(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        let mut state = self.state()?;
        if state.phase != AUTHORITY_RESETTING || state.active != 0 {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
            windowed: false,
            remembers_files: false,
            window_effect: note_window_effect(self),
        };
        drop(state);
        forget_window_files_or_fail(self);
        Ok(operation)
    }

    pub(crate) fn has_reset_pending_directory_creates(&self) -> bool {
        let state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.directory_creations.values().any(|record| {
            record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
        })
    }

    pub(crate) fn directory_create_is_external(&self, token: &DirectoryCreateEffectToken) -> bool {
        let state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .directory_creations
            .get(&token.id)
            .is_some_and(|record| !record.parent.is_managed_root_descendant())
    }

    pub(crate) fn retire_reset_pending_directory_creates(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if state.phase != AUTHORITY_RESETTING || state.active == 0 {
            return Err(stale_capability());
        }
        let ids = state
            .directory_creations
            .iter()
            .filter_map(|(id, record)| {
                (record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            state
                .directory_creations
                .remove(&id)
                .expect("reset-pending directory create remains registered");
            state.release_effect(operation);
        }
        Ok(())
    }

    pub(crate) fn identity(&self, physical: platform::Identity) -> DirectoryIdentity {
        DirectoryIdentity {
            session: self.session_nonce,
            physical,
        }
    }

    pub(crate) fn ensure_leaf_not_directory_create_reserved(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self.state()?;
        if state.directory_creations.values().any(|record| {
            record.parent.inner.identity == parent.inner.identity
                && leaf_names_equivalent(record.name.as_os_str(), name.as_os_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory name is reserved by an unsettled creation",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pause_directory_open_after_precheck(&self, parent: &Directory, name: &LeafName) {
        let pause = self
            .directory_open_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|pause| {
                pause.parent == parent.inner.identity
                    && leaf_names_equivalent(pause.name.as_os_str(), name.as_os_str())
            })
            .map(|pause| (Arc::clone(&pause.prechecked), Arc::clone(&pause.resume)));
        if let Some((prechecked, resume)) = pause {
            prechecked.wait();
            resume.wait();
        }
    }

    #[cfg(test)]
    pub(crate) fn pause_file_observation_after_open(
        &self,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        let mut slot = self.file_observation_pause.lock().unwrap();
        let pause = if slot
            .as_ref()
            .is_some_and(|pause| pause.parent == parent.inner.identity && pause.name == *name)
        {
            slot.take()
        } else {
            None
        };
        drop(slot);
        if let Some(pause) = pause {
            pause
                .opened
                .try_send(())
                .map_err(|_| io::Error::other("file observation test receiver unavailable"))?;
            pause
                .resume
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "file observation test release unavailable",
                    )
                })?;
        }
        Ok(())
    }

    pub(crate) fn ensure_leaf_not_transient_reserved(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self.state()?;
        if transient::transient_leaf_is_reserved(&state, parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file name is reserved by an unsettled transient effect",
            ));
        }
        Ok(())
    }

    pub(crate) fn ensure_leaf_not_root_control(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self.state()?;
        if parent.inner.identity.physical == state.root_identity
            && leaf_names_equivalent(name.as_os_str(), OsStr::new(ROOT_LEASE_NAME))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "root recovery control is reserved by the filesystem authority",
            ));
        }
        Ok(())
    }

    pub(crate) fn register_stage_record(
        self: &Arc<Self>,
        parent: Directory,
        name: LeafName,
        identity: platform::Identity,
        cleanup: platform::FileCleanupHandle,
        creation: &mut StageCreateToken,
        operation: &CapabilityOperation,
    ) -> io::Result<StageToken> {
        if !creation.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || creation.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        let creation_header = state.stage_creations.get(&creation.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
        })?;
        if !creation_header.checked_out
            || creation_header.parent.inner.identity != parent.inner.identity
            || !leaf_names_equivalent(creation_header.name.as_os_str(), name.as_os_str())
        {
            return Err(stale_capability());
        }
        let recovery = creation_header.recovery;
        if state.namespace_footprint_is_reserved(FootprintQuery {
            leaves: &[(&parent, &name)],
            file: Some(identity),
            excluded_stage_create: Some(creation.id),
            ..FootprintQuery::default()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by an unsettled filesystem effect",
            ));
        }
        let id = state.allocate_id()?;
        debug_assert!(state.outstanding_effects > 0);
        state.stages.insert(
            id,
            StageRecord {
                parent,
                name,
                identity,
                cleanup: Some(cleanup),
                phase: StageRegistryPhase::Writing,
                carrier: StageCarrierState::Live,
                promotion: None,
                recovery,
            },
        );
        let removed = state
            .stage_creations
            .remove(&creation.id)
            .expect("prevalidated stage create header remains registered");
        assert!(removed.checked_out);
        creation.armed = false;
        Ok(StageToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    pub(crate) fn reserve_stage_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
        recovery: Option<recovery::RecoveryRegistration>,
    ) -> io::Result<StageCreateToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        if state.namespace_leaf_is_reserved(parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by an unsettled filesystem effect",
            ));
        }
        let id = state.allocate_id()?;
        state.reserve_effect()?;
        state.stage_creations.insert(
            id,
            StageCreateRecord {
                parent: parent.clone(),
                name: name.clone(),
                created: None,
                cleanup: None,
                identity: None,
                phase: StageCreatePhase::Reserved,
                checked_out: false,
                recovery,
                recovery_intent: None,
            },
        );
        Ok(StageCreateToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    pub(crate) fn attach_stage_create(&self, token: &StageCreateToken, created: File) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .stage_creations
            .get_mut(&token.id)
            .expect("admitted stage create reservation remains registered");
        record.created = Some(created);
        record.phase = StageCreatePhase::Applied;
    }

    pub(crate) fn take_stage_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &StageCreateToken,
    ) -> io::Result<StageCreateRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let header = state.stage_creations.get_mut(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
        })?;
        if header.checked_out {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage create settlement is already in progress",
            ));
        }
        header.checked_out = true;
        let record = StageCreateRecord {
            parent: header.parent.clone(),
            name: header.name.clone(),
            created: header.created.take(),
            cleanup: header.cleanup.take(),
            identity: header.identity,
            phase: header.phase,
            checked_out: true,
            recovery: header.recovery,
            recovery_intent: header.recovery_intent.clone(),
        };
        Ok(StageCreateRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    pub(crate) fn abandon_stage_create(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.stage_creations.get_mut(&id) {
            record.phase = StageCreatePhase::Abandoned;
        }
    }

    pub(crate) fn reserve_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<DirectoryCreateEffectToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        if state.namespace_leaf_is_reserved(parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory name is reserved by an unsettled filesystem effect",
            ));
        }
        let id = state.allocate_id()?;
        state.reserve_effect()?;
        state.directory_creations.insert(
            id,
            DirectoryCreateEffectRecord {
                parent: parent.clone(),
                name: name.clone(),
                created: None,
                cleanup: None,
                identity: None,
                phase: DirectoryCreateEffectPhase::Reserved,
                checked_out: false,
            },
        );
        Ok(DirectoryCreateEffectToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    pub(crate) fn attach_directory_create(
        &self,
        token: &DirectoryCreateEffectToken,
        created: platform::DirectoryHandle,
    ) {
        let identity = platform::directory_identity(&created).ok();
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .expect("admitted directory create reservation remains registered");
        record.created = Some(created);
        record.identity = identity;
        record.phase = DirectoryCreateEffectPhase::Applied;
    }

    pub(crate) fn mark_directory_create_unclassified(&self, token: &DirectoryCreateEffectToken) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .expect("admitted directory create reservation remains registered");
        record.phase = DirectoryCreateEffectPhase::CreatedUnclassified;
    }

    pub(crate) fn acknowledge_unclassified_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut DirectoryCreateEffectToken,
    ) -> io::Result<()> {
        let guard = self.take_directory_create(operation, token)?;
        if !matches!(
            guard.record().phase,
            DirectoryCreateEffectPhase::CreatedUnclassified
                | DirectoryCreateEffectPhase::UnclassifiedRecovery
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory creation is not an unclassified preservation",
            ));
        }
        guard.disarm(token, operation);
        Ok(())
    }

    pub(crate) fn take_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &DirectoryCreateEffectToken,
    ) -> io::Result<DirectoryCreateEffectGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let header = state
            .directory_creations
            .get_mut(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
        if header.checked_out {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory create settlement is already in progress",
            ));
        }
        header.checked_out = true;
        let record = DirectoryCreateEffectRecord {
            parent: header.parent.clone(),
            name: header.name.clone(),
            created: header.created.take(),
            cleanup: header.cleanup.take(),
            identity: header.identity,
            phase: header.phase,
            checked_out: true,
        };
        Ok(DirectoryCreateEffectGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    pub(crate) fn abandon_directory_create(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.directory_creations.get_mut(&id) {
            record.phase = match record.phase {
                DirectoryCreateEffectPhase::CreatedUnclassified
                | DirectoryCreateEffectPhase::UnclassifiedRecovery => {
                    DirectoryCreateEffectPhase::UnclassifiedAbandoned
                }
                phase => phase,
            };
            if matches!(
                record.phase,
                DirectoryCreateEffectPhase::Reserved | DirectoryCreateEffectPhase::Applied
            ) {
                record.phase = DirectoryCreateEffectPhase::Abandoned;
            }
        }
    }

    pub(crate) fn ensure_park_available(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        original_name: &LeafName,
        park_name: &LeafName,
        directory_identity: Option<platform::Identity>,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self.state()?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if state.namespace_footprint_is_reserved(FootprintQuery {
            leaves: &[(parent, original_name), (parent, park_name)],
            subtree: directory_identity.map(|identity| (identity, parent)),
            ..FootprintQuery::default()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "park ownership is already retained",
            ));
        }
        Ok(())
    }

    pub(crate) fn reserve_park_registration(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        footprint: FootprintQuery<'_>,
        conflict: &'static str,
    ) -> io::Result<(std::sync::MutexGuard<'_, OperationState>, u64)> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if state.namespace_footprint_is_reserved(footprint) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, conflict));
        }
        let id = state.allocate_id()?;
        state.reserve_effect()?;
        Ok((state, id))
    }

    pub(crate) fn reserve_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        request: &FileParkRequest,
        park_name: LeafName,
        cleanup: platform::FileCleanupHandle,
    ) -> io::Result<FileParkRegistryToken> {
        self.register_file_park(
            operation,
            &request.file.parent,
            request.file.name.clone(),
            park_name,
            request.file.identity,
            request.expected.revision.size,
            request.expected.revision.stamp,
            Some(request.expected.sha256),
            cleanup,
            FileParkRegistryPhase::Reserved,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        identity: platform::Identity,
        size: u64,
        stamp: platform::FileStamp,
        expected_digest: Option<[u8; 32]>,
        cleanup: platform::FileCleanupHandle,
        phase: FileParkRegistryPhase,
    ) -> io::Result<FileParkRegistryToken> {
        let (mut state, id) = self.reserve_park_registration(
            operation,
            FootprintQuery {
                leaves: &[(parent, &original_name), (parent, &park_name)],
                file: Some(identity),
                ..FootprintQuery::default()
            },
            "file park name is reserved by an unsettled filesystem effect",
        )?;
        assert!(
            state
                .file_parks
                .insert(
                    id,
                    FileParkRegistryRecord {
                        parent: parent.clone(),
                        original_name,
                        name: park_name,
                        identity,
                        size,
                        stamp,
                        expected_digest,
                        cleanup: Some(cleanup),
                        phase,
                        linked_effect: None,
                    },
                )
                .is_none()
        );
        Ok(FileParkRegistryToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    pub(crate) fn take_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &FileParkRegistryToken,
    ) -> io::Result<FileParkRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let header = state
            .file_parks
            .get_mut(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        if header.linked_effect.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park is linked to an unsettled replacement",
            ));
        }
        let cleanup = header.cleanup.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park is already checked out",
            )
        })?;
        let record = FileParkSettlementRecord {
            parent: header.parent.clone(),
            original_name: header.original_name.clone(),
            name: header.name.clone(),
            identity: header.identity,
            size: header.size,
            stamp: header.stamp,
            expected_digest: header.expected_digest,
            cleanup,
            phase: header.phase,
        };
        drop(state);
        Ok(FileParkRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    pub(crate) fn rollback_file_park_registration(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut FileParkRegistryToken,
    ) -> io::Result<()> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let record = state
            .file_parks
            .remove(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        if record.linked_effect.is_some() || record.cleanup.is_none() {
            state.file_parks.insert(token.id, record);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park authority is not available for rollback",
            ));
        }
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    pub(crate) fn reserve_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        directory: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        cleanup: platform::DirectoryCleanupHandle,
    ) -> io::Result<DirectoryParkRegistryToken> {
        self.register_directory_park(
            operation,
            parent,
            original_name,
            park_name,
            directory.inner.identity.physical,
            cleanup,
            DirectoryParkRegistryPhase::Reserved,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        identity: platform::Identity,
        cleanup: platform::DirectoryCleanupHandle,
        phase: DirectoryParkRegistryPhase,
    ) -> io::Result<DirectoryParkRegistryToken> {
        let (mut state, id) = self.reserve_park_registration(
            operation,
            FootprintQuery {
                leaves: &[(parent, &original_name), (parent, &park_name)],
                subtree: Some((identity, parent)),
                ..FootprintQuery::default()
            },
            "directory park name is reserved by an unsettled filesystem effect",
        )?;
        assert!(
            state
                .directory_parks
                .insert(
                    id,
                    DirectoryParkRegistryRecord {
                        parent: parent.clone(),
                        original_name,
                        name: park_name,
                        identity,
                        cleanup: Some(cleanup),
                        phase,
                    },
                )
                .is_none()
        );
        Ok(DirectoryParkRegistryToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    pub(crate) fn take_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<DirectoryParkRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let header = state.directory_parks.get_mut(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
        })?;
        let cleanup = header.cleanup.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory park is already checked out",
            )
        })?;
        let record = DirectoryParkSettlementRecord {
            parent: header.parent.clone(),
            original_name: header.original_name.clone(),
            name: header.name.clone(),
            identity: header.identity,
            cleanup,
            phase: header.phase,
        };
        drop(state);
        Ok(DirectoryParkRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    pub(crate) fn rollback_directory_park_registration(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut DirectoryParkRegistryToken,
    ) -> io::Result<()> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
        let record = state.directory_parks.remove(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
        })?;
        if record.cleanup.is_none() {
            state.directory_parks.insert(token.id, record);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory park authority is not available for rollback",
            ));
        }
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    pub(crate) fn abandon_file_park(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.file_parks.get_mut(&id) {
            record.phase = FileParkRegistryPhase::Abandoned;
        }
    }

    pub(crate) fn abandon_directory_park(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.directory_parks.get_mut(&id) {
            record.phase = DirectoryParkRegistryPhase::Abandoned;
        }
    }

    pub(crate) fn abandon_stage(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.stages.get_mut(&id) {
            record.carrier = StageCarrierState::Abandoned;
        }
    }

    pub(crate) fn update_stage(&self, id: u64, phase: StageRegistryPhase) -> io::Result<()> {
        let mut state = self.state()?;
        let displaced_park = state
            .stages
            .get(&id)
            .and_then(|record| record.promotion.as_ref())
            .and_then(|promotion| promotion.displaced_park);
        if matches!(
            phase,
            StageRegistryPhase::Writing | StageRegistryPhase::Sealed
        ) && let Some(park_id) = displaced_park
        {
            let park = state.file_parks.get_mut(&park_id).ok_or_else(|| {
                io::Error::other("replacement park link lost its registry header")
            })?;
            if park.linked_effect != Some(FileParkLink::Stage(id)) {
                return Err(io::Error::other("replacement park link is inconsistent"));
            }
            park.linked_effect = None;
        }
        let record = state.stages.get_mut(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        record.phase = phase;
        if matches!(
            phase,
            StageRegistryPhase::Writing | StageRegistryPhase::Sealed
        ) {
            record.promotion = None;
        }
        Ok(())
    }

    pub(crate) fn prepare_stage_promotion(
        &self,
        id: u64,
        destination: Directory,
        name: LeafName,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
        displaced_park: Option<&FileParkRegistryToken>,
        expected_recovery: Option<&RecoveryRecord>,
    ) -> io::Result<()> {
        let mut state = self.state()?;
        if !(matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
            || state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        if !receipt.matches_attempt(attempt_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "publication receipt does not match the allocated stage attempt",
            ));
        }
        let displaced_park_id = state.file_park_handoff_id(
            displaced_park,
            self,
            &NamespaceLeaf {
                parent: destination.clone(),
                name: name.clone(),
            },
            None,
        )?;
        if state.namespace_footprint_is_reserved(FootprintQuery {
            leaves: &[(&destination, &name)],
            excluded_file_park_original: displaced_park_id,
            excluded_recovery_target_stage: Some(id),
            ..FootprintQuery::default()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "promotion destination is reserved by an unsettled filesystem effect",
            ));
        }
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.phase != StageRegistryPhase::Sealed || record.promotion.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage is not ready for a new publication attempt",
            ));
        }
        prepare_recovery_publication(&self.lease, &mut state, id, expected_recovery)?;
        let record = state
            .stages
            .get_mut(&id)
            .expect("prevalidated stage remains registered");
        record.promotion = Some(StagePromotionRecord {
            destination: NamespaceLeaf {
                parent: destination,
                name,
            },
            attempt_id,
            receipt,
            displaced_park: displaced_park_id,
        });
        record.phase = StageRegistryPhase::PromotionAttempted;
        if let Some(park_id) = displaced_park_id {
            let park = state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated replacement park remains registered");
            assert!(park.linked_effect.is_none());
            park.linked_effect = Some(FileParkLink::Stage(id));
        }
        Ok(())
    }

    pub(crate) fn record_stage_publication(
        &self,
        id: u64,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
    ) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = state.stages.get_mut(&id) else {
            std::process::abort();
        };
        if record.phase != StageRegistryPhase::PromotionAttempted {
            std::process::abort();
        }
        let Some(promotion) = record.promotion.as_mut() else {
            std::process::abort();
        };
        if promotion.attempt_id != attempt_id
            || !receipt.matches_attempt(attempt_id)
            || !promotion.receipt.accepts_successor(&receipt)
        {
            std::process::abort();
        }
        promotion.receipt = receipt;
    }

    pub(crate) fn validate_stage_publication(
        &self,
        id: u64,
        attempt_id: u64,
        receipt: &platform::PublicationReceipt,
    ) -> io::Result<()> {
        let state = self.state()?;
        let record = state.stages.get(&id).ok_or_else(stale_capability)?;
        let promotion = record.promotion.as_ref().ok_or_else(stale_capability)?;
        if record.phase != StageRegistryPhase::PromotionAttempted
            || promotion.attempt_id != attempt_id
            || !receipt.matches_attempt(attempt_id)
            || !promotion.receipt.accepts_successor(receipt)
        {
            return Err(stale_capability());
        }
        Ok(())
    }

    pub(crate) fn allocate_stage_publication_attempt(&self, id: u64) -> io::Result<u64> {
        let mut state = self.state()?;
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.phase != StageRegistryPhase::Sealed || record.promotion.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage is not ready for a new publication attempt",
            ));
        }
        state.allocate_id()
    }

    pub(crate) fn disarm_stage(
        self: &Arc<Self>,
        id: u64,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        assert!(Arc::ptr_eq(&operation.authority, self));
        let mut state = self.state()?;
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.cleanup.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage settlement is already in progress",
            ));
        }
        let displaced_park = record
            .promotion
            .as_ref()
            .and_then(|promotion| promotion.displaced_park);
        if let Some(park_id) = displaced_park {
            let park = state.file_parks.get(&park_id).ok_or_else(|| {
                io::Error::other("replacement park link lost its registry header")
            })?;
            if park.linked_effect != Some(FileParkLink::Stage(id)) {
                return Err(io::Error::other("replacement park link is inconsistent"));
            }
        }
        state
            .stages
            .remove(&id)
            .expect("prevalidated stage remains registered");
        if let Some(park_id) = displaced_park {
            let park = state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated replacement park remains registered");
            park.linked_effect = None;
        }
        state.release_effect(operation);
        Ok(())
    }

    pub(crate) fn cleanup_stage(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.state()?;
            if !(matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
                || state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self))
            {
                return Err(stale_capability());
            }
            if state.recovery.is_uncertain() {
                state.recovery.reconcile_uncertain(&self.lease)?;
            }
            if !state.stages.contains_key(&id) {
                return Ok(());
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .stages
                .get_mut(&id)
                .expect("prevalidated stage header remains registered");
            let cleanup = header.cleanup.take().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "stage cleanup is already in progress",
                )
            })?;
            let record = StageRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                identity: header.identity,
                cleanup: Some(cleanup),
                phase: header.phase,
                carrier: header.carrier,
                promotion: header
                    .promotion
                    .as_ref()
                    .map(|promotion| StagePromotionRecord {
                        destination: NamespaceLeaf {
                            parent: promotion.destination.parent.clone(),
                            name: promotion.destination.name.clone(),
                        },
                        attempt_id: promotion.attempt_id,
                        receipt: promotion.receipt.clone(),
                        displaced_park: promotion.displaced_park,
                    }),
                recovery: header.recovery,
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                    windowed: false,
                    remembers_files: false,
                    window_effect: note_window_effect(self),
                },
            )
        };
        forget_window_files_or_fail(self);
        let mut expected_recovery = None;
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| match record.phase {
                StageRegistryPhase::PromotionAttempted | StageRegistryPhase::Unresolved => {
                    let promotion = record.promotion.as_mut().ok_or_else(|| {
                        io::Error::other("promotion stage lost its destination authority")
                    })?;
                    let destination = &promotion.destination;
                    destination.parent.validate(&operation)?;
                    let source = platform::file_binding_state(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        record.identity,
                    )?;
                    let published = platform::file_binding_state(
                        destination.parent.inner.handle(),
                        destination.name.as_os_str(),
                        record.identity,
                    )?;
                    match (source, published) {
                        (
                            platform::BindingState::Exact,
                            platform::BindingState::Absent | platform::BindingState::Occupied,
                        ) if promotion.receipt.is_attempted() => {
                            expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                            platform::remove_parked_file(
                                record.parent.inner.handle(),
                                record.name.as_os_str(),
                                record
                                    .cleanup
                                    .as_mut()
                                    .expect("checked-out stage retains cleanup authority"),
                                record.identity,
                            )
                        }
                        (platform::BindingState::Absent, platform::BindingState::Exact) => {
                            expected_recovery =
                                selected_recovery_stage_record(self, record.recovery)?;
                            platform::settle_parked_publication(
                                &mut promotion.receipt,
                                promotion.attempt_id,
                                record
                                    .cleanup
                                    .as_ref()
                                    .expect("checked-out stage retains cleanup authority"),
                                record.parent.inner.handle(),
                                record.name.as_os_str(),
                                destination.parent.inner.handle(),
                                destination.name.as_os_str(),
                            )?;
                            destination.parent.validate(&operation)
                        }
                        _ => Err(identity_changed(
                            "promotion-attempted stage topology is indeterminate",
                        )),
                    }
                }
                StageRegistryPhase::CleanupAttempted => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    if platform::settle_removed_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_ref()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                    .is_ok()
                    {
                        Ok(())
                    } else {
                        platform::remove_parked_file(
                            record.parent.inner.handle(),
                            record.name.as_os_str(),
                            record
                                .cleanup
                                .as_mut()
                                .expect("checked-out stage retains cleanup authority"),
                            record.identity,
                        )
                    }
                }
                StageRegistryPhase::Writing => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    platform::remove_parked_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_mut()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                }
                StageRegistryPhase::Sealed => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    platform::remove_parked_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_mut()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                }
            })
            .and_then(|()| {
                settle_removed_recovery_stage(self, &operation, &record, expected_recovery.as_ref())
            });
        if let Err(error) = result {
            record.phase = match record.phase {
                StageRegistryPhase::PromotionAttempted | StageRegistryPhase::Unresolved => {
                    StageRegistryPhase::Unresolved
                }
                _ => StageRegistryPhase::CleanupAttempted,
            };
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let header = state
                .stages
                .get_mut(&id)
                .expect("checked-out stage header remains registered");
            assert!(header.cleanup.is_none());
            header.cleanup = record.cleanup;
            header.phase = record.phase;
            header.carrier = record.carrier;
            header.promotion = record.promotion;
            header.recovery = record.recovery;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .stages
                .remove(&id)
                .expect("settled stage header remains registered");
            assert!(removed.cleanup.is_none());
            if let Some(park_id) = removed
                .promotion
                .as_ref()
                .and_then(|promotion| promotion.displaced_park)
            {
                let park = state
                    .file_parks
                    .get_mut(&park_id)
                    .expect("linked replacement park remains registered");
                assert_eq!(park.linked_effect, Some(FileParkLink::Stage(id)));
                park.linked_effect = None;
            }
            state.release_effect(&operation);
        }
        Ok(())
    }

    pub(crate) fn cleanup_abandoned_stage_create(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.state()?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let header = state.stage_creations.get(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
            })?;
            if !matches!(
                header.phase,
                StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
            ) || header.checked_out
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "stage create authority is still live",
                ));
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .stage_creations
                .get_mut(&id)
                .expect("prevalidated stage create header remains registered");
            header.checked_out = true;
            let record = StageCreateRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                created: header.created.take(),
                cleanup: header.cleanup.take(),
                identity: header.identity,
                phase: header.phase,
                checked_out: true,
                recovery: header.recovery,
                recovery_intent: header.recovery_intent.clone(),
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                    windowed: false,
                    remembers_files: false,
                    window_effect: note_window_effect(self),
                },
            )
        };
        forget_window_files_or_fail(self);
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| {
                if record.cleanup.is_none() {
                    let created = record
                        .created
                        .as_ref()
                        .expect("abandoned stage create retains its created file");
                    let identity = platform::file_identity(created)?;
                    record.cleanup = Some(platform::clone_stage_cleanup(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        created,
                        identity,
                    )?);
                    record.identity = Some(identity);
                    drop(record.created.take());
                }
                let cleanup = record
                    .cleanup
                    .as_mut()
                    .expect("abandoned stage create retains cleanup authority");
                let identity = record
                    .identity
                    .ok_or_else(|| identity_changed("stage create identity is absent"))?;
                if record.phase == StageCreatePhase::CleanupAttempted
                    && platform::settle_removed_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                    .is_ok()
                {
                    Ok(())
                } else {
                    platform::remove_parked_file(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                }
            })
            .and_then(|()| settle_removed_recovery_stage_create(self, &operation, &record));
        if let Err(error) = result {
            record.phase = StageCreatePhase::CleanupAttempted;
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let header = state
                .stage_creations
                .get_mut(&id)
                .expect("checked-out stage create header remains registered");
            assert!(header.checked_out);
            header.created = record.created;
            header.cleanup = record.cleanup;
            header.identity = record.identity;
            header.phase = record.phase;
            header.checked_out = false;
            header.recovery = record.recovery;
            header.recovery_intent = record.recovery_intent;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .stage_creations
                .remove(&id)
                .expect("settled stage create header remains registered");
            assert!(removed.checked_out);
            state.release_effect(&operation);
        }
        Ok(())
    }

    pub(crate) fn cleanup_abandoned_directory_create(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.state()?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let header = state.directory_creations.get(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
            if !matches!(
                header.phase,
                DirectoryCreateEffectPhase::Abandoned
                    | DirectoryCreateEffectPhase::CleanupAttempted
            ) || header.checked_out
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "directory create authority is still live",
                ));
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .directory_creations
                .get_mut(&id)
                .expect("prevalidated directory create header remains registered");
            header.checked_out = true;
            let record = DirectoryCreateEffectRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                created: header.created.take(),
                cleanup: header.cleanup.take(),
                identity: header.identity,
                phase: header.phase,
                checked_out: true,
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                    windowed: false,
                    remembers_files: false,
                    window_effect: note_window_effect(self),
                },
            )
        };
        forget_window_files_or_fail(self);
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| {
                if record.cleanup.is_none() {
                    let created = record
                        .created
                        .as_ref()
                        .expect("abandoned directory create retains its created directory");
                    let identity = platform::directory_identity(created)?;
                    record.cleanup = Some(platform::open_parked_directory(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        identity,
                    )?);
                    record.identity = Some(identity);
                    drop(record.created.take());
                }
                let cleanup = record
                    .cleanup
                    .as_mut()
                    .expect("abandoned directory create retains cleanup authority");
                let identity = record
                    .identity
                    .ok_or_else(|| identity_changed("directory create identity is absent"))?;
                if record.phase == DirectoryCreateEffectPhase::CleanupAttempted
                    && platform::settle_removed_directory(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                    .is_ok()
                {
                    Ok(())
                } else {
                    platform::remove_parked_directory(
                        record.parent.inner.handle(),
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                }
            });
        if let Err(error) = result {
            record.phase = DirectoryCreateEffectPhase::CleanupAttempted;
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let header = state
                .directory_creations
                .get_mut(&id)
                .expect("checked-out directory create header remains registered");
            assert!(header.checked_out);
            header.created = record.created;
            header.cleanup = record.cleanup;
            header.identity = record.identity;
            header.phase = record.phase;
            header.checked_out = false;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .directory_creations
                .remove(&id)
                .expect("settled directory create header remains registered");
            assert!(removed.checked_out);
            state.release_effect(&operation);
        }
        Ok(())
    }

    pub(crate) fn begin_terminal_drain(&self, require_empty_recovery: bool) -> io::Result<()> {
        {
            let mut state = self.state()?;
            if state.phase != AUTHORITY_LIVE {
                return Err(stale_capability());
            }
            if state.recovery.is_uncertain()
                || require_empty_recovery && state.recovery.has_live_or_uncertain()
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "root recovery control retains unsettled ownership",
                ));
            }
            if state.active != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem session still has active capability operations",
                ));
            }
            state.phase = AUTHORITY_QUIESCING;
        }
        let mut quiescing = TerminalQuiescingRollback::new(self);

        let owners = {
            let state = self.state()?;
            if state.phase != AUTHORITY_QUIESCING {
                return Err(stale_capability());
            }
            state
                .active_effect_owners
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for owner in &owners {
            owner.settle(true)?;
        }
        drop(owners);

        let disposal = {
            let mut state = self.state()?;
            if state.phase != AUTHORITY_QUIESCING {
                return Err(stale_capability());
            }
            if state.active != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect settlement remained active during terminal drain",
                ));
            }
            if state
                .active_effect_owners
                .values()
                .any(|owner| owner.has_domain_pending())
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by a domain-sensitive effect",
                ));
            }
            if !state.moves.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by an unowned move effect",
                ));
            }
            state
                .effect_owner_handles
                .retain(|_, owner| owner.strong_count() > 0);
            let has_external_owner = state.effect_owner_handles.iter().any(|(id, owner)| {
                let authority_owned = usize::from(state.active_effect_owners.contains_key(id));
                owner.strong_count() > authority_owned
            });
            if has_external_owner {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by a live effect owner",
                ));
            }
            let owners = state
                .active_effect_owners
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut disposal = Vec::new();
            for owner in owners {
                disposal.extend(owner.take_for_terminal_disposal());
            }
            state.active_effect_owners.clear();
            disposal
        };
        drop(disposal);

        let mut state = self.state()?;
        if state.phase != AUTHORITY_QUIESCING {
            return Err(stale_capability());
        }
        if state.active != 0 || !state.active_effect_owners.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect cleanup raced with terminal drain",
            ));
        }
        validate_terminal_registry_state(&state)?;
        state.phase = AUTHORITY_DRAINING;
        quiescing.disarm();
        Ok(())
    }

    pub(crate) fn restore_live_after_quiescing(&self) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase == AUTHORITY_QUIESCING {
            state.phase = AUTHORITY_LIVE;
        }
    }

    pub(crate) fn try_finish_terminal_drain(
        self: &Arc<Self>,
        terminal_phase: u8,
        validate_owned_root: bool,
    ) -> io::Result<SessionDrainSettlement> {
        let (cleanup_ids, create_cleanup_ids, directory_create_cleanup_ids, transient_cleanup_ids) = {
            let state = self.state()?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            if state.active != 0 || !state.moves.is_empty() {
                return Ok(SessionDrainSettlement::Pending);
            }
            (
                state
                    .stages
                    .iter()
                    .filter_map(|(id, record)| {
                        (record.carrier == StageCarrierState::Abandoned).then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .stage_creations
                    .iter()
                    .filter_map(|(id, record)| {
                        matches!(
                            record.phase,
                            StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
                        )
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .directory_creations
                    .iter()
                    .filter_map(|(id, record)| {
                        matches!(
                            record.phase,
                            DirectoryCreateEffectPhase::Abandoned
                                | DirectoryCreateEffectPhase::CleanupAttempted
                        )
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .transients
                    .iter()
                    .filter_map(|(id, record)| {
                        (record.phase == transient::TransientEffectPhase::Abandoned).then_some(*id)
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if validate_owned_root {
            platform::validate_lease(&self.lease)?;
            platform::validate_root(&self.root)?;
            self.validate_retained_process_image_outside_root()?;
        }
        for id in cleanup_ids {
            let _ = self.cleanup_stage(id);
        }
        for id in create_cleanup_ids {
            let _ = self.cleanup_abandoned_stage_create(id);
        }
        for id in directory_create_cleanup_ids {
            let _ = self.cleanup_abandoned_directory_create(id);
        }
        let mut transient_cleanup_blocked = false;
        for id in transient_cleanup_ids {
            if self.cleanup_abandoned_transient(id).is_err() {
                transient_cleanup_blocked = true;
            }
        }
        let mut state = self.state()?;
        if state.active != 0
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || state.directory_creations.values().any(|record| {
                record.phase != DirectoryCreateEffectPhase::UnclassifiedAbandoned
                    && !(terminal_phase == AUTHORITY_RESETTING
                        && record.phase
                            == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending)
            })
            || transient_cleanup_blocked
            || !state.transients.is_empty()
            || state.recovery.is_checked_out()
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        let abandoned_count = state
            .file_parks
            .values()
            .filter(|record| record.phase == FileParkRegistryPhase::Abandoned)
            .count()
            .checked_add(
                state
                    .directory_parks
                    .values()
                    .filter(|record| record.phase == DirectoryParkRegistryPhase::Abandoned)
                    .count(),
            )
            .and_then(|count| {
                count.checked_add(
                    state
                        .directory_creations
                        .values()
                        .filter(|record| {
                            record.phase == DirectoryCreateEffectPhase::UnclassifiedAbandoned
                        })
                        .count(),
                )
            })
            .ok_or_else(|| io::Error::other("abandoned effect recovery count overflowed"))?;
        debug_assert!(abandoned_count <= MAX_OUTSTANDING_EFFECTS);
        if abandoned_count != 0 {
            let mut files = Vec::new();
            let mut directories = Vec::new();
            let mut directory_create_preservations = Vec::new();
            let mut permits = Vec::with_capacity(abandoned_count);
            for (id, record) in &mut state.file_parks {
                if record.phase != FileParkRegistryPhase::Abandoned {
                    continue;
                }
                record.phase = FileParkRegistryPhase::Live;
                files.push(ParkedFile {
                    parent: record.parent.clone(),
                    original_name: record.original_name.clone(),
                    park_name: record.name.clone(),
                    identity: record.identity,
                    size: record.size,
                    stamp: record.stamp,
                    verified: record.expected_digest.is_none(),
                    token: FileParkRegistryToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                    authority: Arc::downgrade(self),
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::File(*id),
                });
            }
            for (id, record) in &mut state.directory_parks {
                if record.phase != DirectoryParkRegistryPhase::Abandoned {
                    continue;
                }
                record.phase = DirectoryParkRegistryPhase::Live;
                directories.push(ParkedDirectory {
                    parent: record.parent.clone(),
                    original_name: record.original_name.clone(),
                    park_name: record.name.clone(),
                    identity: DirectoryIdentity {
                        session: self.session_nonce,
                        physical: record.identity,
                    },
                    token: DirectoryParkRegistryToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                    authority: Arc::downgrade(self),
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::Directory(*id),
                });
            }
            for (id, record) in &mut state.directory_creations {
                if record.phase != DirectoryCreateEffectPhase::UnclassifiedAbandoned {
                    continue;
                }
                record.phase = DirectoryCreateEffectPhase::UnclassifiedRecovery;
                directory_create_preservations.push(DirectoryCreatePreservation {
                    token: DirectoryCreateEffectToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::DirectoryCreate(*id),
                });
            }
            return Ok(SessionDrainSettlement::Recovery {
                recovery: SessionDrainRecoveryState {
                    files,
                    directories,
                    directory_create_preservations,
                    file_removals: Vec::new(),
                    file_restores: Vec::new(),
                    directory_removals: Vec::new(),
                    directory_restores: Vec::new(),
                },
                permits,
            });
        }
        let reset_pending_count = state
            .directory_creations
            .values()
            .filter(|record| {
                record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
            })
            .count();
        let expected_outstanding = if terminal_phase == AUTHORITY_RESETTING {
            reset_pending_count
        } else {
            0
        };
        if terminal_phase != AUTHORITY_RESETTING && state.outstanding_effects != 0 {
            return Ok(SessionDrainSettlement::Pending);
        }
        if !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.transients.is_empty()
            || state.recovery.is_checked_out()
            || state.outstanding_effects != expected_outstanding
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        drop(state);
        if validate_owned_root {
            self.validate_retained_process_image_outside_root()?;
        }
        let mut state = self.state()?;
        if state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        if terminal_phase == AUTHORITY_RESETTING && state.recovery.has_live_or_uncertain() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "root reset is blocked by durable recovery ownership",
            ));
        }
        let directory_creations_settled = state.directory_creations.is_empty()
            || (terminal_phase == AUTHORITY_RESETTING
                && state.directory_creations.len() == reset_pending_count
                && state.directory_creations.values().all(|record| {
                    record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
                }));
        if state.active != 0
            || state.outstanding_effects != expected_outstanding
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !directory_creations_settled
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.transients.is_empty()
            || state.recovery.is_checked_out()
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        state.phase = terminal_phase;
        Ok(SessionDrainSettlement::Ready)
    }
}

/// A namespace footprint to test against every unsettled effect's reservations. The
/// exclusions name the caller's own registry entry whose reservation it inherits.
#[derive(Default)]
pub(crate) struct FootprintQuery<'a> {
    pub(crate) leaves: &'a [(&'a Directory, &'a LeafName)],
    pub(crate) subtree: Option<(platform::Identity, &'a Directory)>,
    pub(crate) file: Option<platform::Identity>,
    pub(crate) excluded_file_park_original: Option<u64>,
    pub(crate) excluded_stage_create: Option<u64>,
    pub(crate) excluded_recovery_target_stage: Option<u64>,
}

impl<'a> FootprintQuery<'a> {
    pub(crate) fn leaves(leaves: &'a [(&'a Directory, &'a LeafName)]) -> Self {
        Self {
            leaves,
            ..Self::default()
        }
    }
}

pub(crate) struct OperationState {
    pub(crate) phase: u8,
    pub(crate) root_identity: platform::Identity,
    pub(crate) active: usize,
    pub(crate) outstanding_effects: usize,
    pub(crate) next_id: u64,
    pub(crate) moves: HashMap<u64, MoveEffectRecord>,
    pub(crate) effect_owner_handles: HashMap<u64, Weak<EffectOwnerState>>,
    pub(crate) active_effect_owners: HashMap<u64, Arc<EffectOwnerState>>,
    pub(crate) stages: HashMap<u64, StageRecord>,
    pub(crate) stage_creations: HashMap<u64, StageCreateRecord>,
    pub(crate) directory_creations: HashMap<u64, DirectoryCreateEffectRecord>,
    pub(crate) file_parks: HashMap<u64, FileParkRegistryRecord>,
    pub(crate) directory_parks: HashMap<u64, DirectoryParkRegistryRecord>,
    pub(crate) transients: HashMap<u64, transient::TransientEffectRecord>,
    pub(crate) recovery: recovery::RecoveryJournal,
    pub(crate) recovery_orphans: Vec<RecoveryOrphan>,
}

impl OperationState {
    pub(crate) fn file_park_handoff_id(
        &self,
        token: Option<&FileParkRegistryToken>,
        authority: *const CapabilityAuthority,
        destination: &NamespaceLeaf,
        moved_file: Option<(platform::Identity, &NamespaceLeaf)>,
    ) -> io::Result<Option<u64>> {
        let Some(token) = token else {
            return Ok(None);
        };
        if !token.armed || token.authority.as_ptr() != authority {
            return Err(stale_capability());
        }
        let park = self
            .file_parks
            .get(&token.id)
            .ok_or_else(stale_capability)?;
        let source_conflicts = moved_file.is_some_and(|(identity, source)| {
            identity == park.identity
                || source.parent.inner.identity == park.parent.inner.identity
                    && (leaf_names_equivalent(
                        source.name.as_os_str(),
                        park.original_name.as_os_str(),
                    ) || leaf_names_equivalent(source.name.as_os_str(), park.name.as_os_str()))
        });
        if park.phase != FileParkRegistryPhase::Live
            || park.linked_effect.is_some()
            || park.cleanup.is_none()
            || park.parent.inner.identity != destination.parent.inner.identity
            || !leaf_names_equivalent(park.original_name.as_os_str(), destination.name.as_os_str())
            || source_conflicts
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "replacement park cannot hand off its original leaf",
            ));
        }
        Ok(Some(token.id))
    }

    pub(crate) fn namespace_footprint_is_reserved(&self, query: FootprintQuery<'_>) -> bool {
        let FootprintQuery {
            leaves: candidate_leaves,
            subtree: candidate_subtree,
            file: candidate_file,
            excluded_file_park_original,
            excluded_stage_create,
            excluded_recovery_target_stage,
        } = query;
        if candidate_leaves.iter().any(|(directory, name)| {
            directory.inner.identity.physical == self.root_identity
                && leaf_names_equivalent(name.as_os_str(), OsStr::new(ROOT_LEASE_NAME))
        }) {
            return true;
        }
        let candidate_conflicts_with_name = |directory: &Directory, name: &OsStr| {
            candidate_leaves
                .iter()
                .any(|(candidate_directory, candidate_name)| {
                    candidate_directory.inner.identity == directory.inner.identity
                        && leaf_names_equivalent(name, candidate_name.as_os_str())
                })
                || candidate_subtree.is_some_and(|(identity, _)| {
                    directory_has_physical_ancestor(directory, identity)
                })
        };
        let candidate_conflicts_with_subtree =
            |identity: platform::Identity, anchor: &Directory| {
                candidate_leaves
                    .iter()
                    .any(|(directory, _)| directory_has_physical_ancestor(directory, identity))
                    || candidate_subtree.is_some_and(|(candidate_identity, candidate_anchor)| {
                        candidate_identity == identity
                            || directory_has_physical_ancestor(candidate_anchor, identity)
                            || directory_has_physical_ancestor(anchor, candidate_identity)
                    })
            };

        let excluded_recovery = excluded_stage_create
            .and_then(|id| self.stage_creations.get(&id))
            .and_then(|record| record.recovery);
        if self.recovery_orphans.iter().any(|orphan| {
            if Some(orphan.registration) == excluded_recovery {
                return false;
            }
            let Some(record) = self.recovery.record(orphan.registration) else {
                return true;
            };
            let Some(parent) = orphan.ancestors.last().copied() else {
                return true;
            };
            let owns_target = record.phase.owns_target();
            let stage = recovery::recovery_stage_leaf(record.operation_id);
            let park = recovery::recovery_park_leaf(record.operation_id);
            let owns_leaf = candidate_leaves.iter().any(|(directory, name)| {
                directory.inner.identity.physical == parent
                    && (leaf_names_equivalent(name.as_os_str(), OsStr::new(stage.as_str()))
                        || recovery_owns_park(record)
                            && leaf_names_equivalent(name.as_os_str(), OsStr::new(park.as_str()))
                        || owns_target
                            && leaf_names_equivalent(
                                name.as_os_str(),
                                OsStr::new(record.destination_leaf.as_str()),
                            ))
            });
            let owns_subtree =
                candidate_subtree.is_some_and(|(identity, _)| orphan.ancestors.contains(&identity));
            let owns_file = candidate_file.is_some_and(|identity| orphan.files.contains(&identity));
            owns_leaf || owns_subtree || owns_file
        }) {
            return true;
        }

        self.moves.values().any(|movement| {
            candidate_file.is_some() && candidate_file == movement.moved_file
                || candidate_conflicts_with_name(
                    &movement.source.parent,
                    movement.source.name.as_os_str(),
                )
                || candidate_conflicts_with_name(
                    &movement.destination.parent,
                    movement.destination.name.as_os_str(),
                )
                || movement.moved_directory.is_some_and(|identity| {
                    candidate_conflicts_with_subtree(identity, &movement.source.parent)
                })
        }) || self.transients.values().any(|record| {
            // The named transient's original reservation is handed to only
            // its exact registered stage. No reservation is removed between
            // validation and native no-replace promotion.
            #[cfg(not(target_os = "linux"))]
            if record.stage_id.is_some()
                && record.stage_id == excluded_recovery_target_stage
                && candidate_file.is_none()
                && candidate_subtree.is_none()
            {
                return false;
            }
            candidate_file.is_some() && candidate_file == record.identity
                || candidate_conflicts_with_name(&record.directory, record.destination.as_os_str())
        }) || self.directory_creations.values().any(|record| {
            candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || record.identity.is_some_and(|identity| {
                    candidate_conflicts_with_subtree(identity, &record.parent)
                })
                // A new child cannot contain its parent or physical ancestors.
                // Keep unknown descendants and every subtree mutation reserved.
                || record.identity.is_none()
                    && (candidate_subtree.is_some()
                        || candidate_leaves.iter().any(|(directory, _)| {
                            !directory_has_physical_ancestor(
                                &record.parent,
                                directory.inner.identity.physical,
                            )
                        }))
        }) || self.stage_creations.iter().any(|(id, record)| {
            Some(*id) != excluded_stage_create
                && (candidate_file.is_some() && candidate_file == record.identity
                    || candidate_conflicts_with_name(&record.parent, record.name.as_os_str()))
        }) || self.file_parks.iter().any(|(id, record)| {
            candidate_file == Some(record.identity)
                || (*id != excluded_file_park_original.unwrap_or(u64::MAX)
                    && candidate_conflicts_with_name(
                        &record.parent,
                        record.original_name.as_os_str(),
                    ))
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
        }) || self.directory_parks.values().any(|record| {
            candidate_conflicts_with_name(&record.parent, record.original_name.as_os_str())
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || candidate_conflicts_with_subtree(record.identity, &record.parent)
        }) || self.stages.iter().any(|(id, record)| {
            candidate_file == Some(record.identity)
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || record.promotion.as_ref().is_some_and(|promotion| {
                    candidate_conflicts_with_name(
                        &promotion.destination.parent,
                        promotion.destination.name.as_os_str(),
                    )
                })
                || (Some(*id) != excluded_recovery_target_stage
                    && record.recovery.is_some_and(|registration| {
                        self.recovery.record(registration).is_none_or(|recovery| {
                            recovery.phase.owns_target()
                                && candidate_conflicts_with_name(
                                    &record.parent,
                                    OsStr::new(recovery.destination_leaf.as_str()),
                                )
                        })
                    }))
        })
    }

    pub(crate) fn namespace_leaf_is_reserved(
        &self,
        candidate_directory: &Directory,
        candidate_name: &LeafName,
    ) -> bool {
        self.namespace_footprint_is_reserved(FootprintQuery::leaves(&[(
            candidate_directory,
            candidate_name,
        )]))
    }

    pub(crate) fn reserve_effect(&mut self) -> io::Result<()> {
        self.reserve_effects(1)
    }

    pub(crate) fn reserve_effects(&mut self, count: usize) -> io::Result<()> {
        let outstanding_effects = self
            .outstanding_effects
            .checked_add(count)
            .ok_or_else(|| io::Error::other("filesystem effect registry capacity overflowed"))?;
        if outstanding_effects > MAX_OUTSTANDING_EFFECTS {
            return Err(io::Error::other(
                "filesystem effect registry capacity is exhausted",
            ));
        }
        self.outstanding_effects = outstanding_effects;
        Ok(())
    }

    pub(crate) fn allocate_id(&mut self) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem registry identity overflowed"))?;
        Ok(id)
    }

    pub(crate) fn reserve_move_effect(&mut self, record: MoveEffectRecord) -> io::Result<u64> {
        let id = self.allocate_id()?;
        self.reserve_effect()?;
        assert!(self.moves.insert(id, record).is_none());
        Ok(id)
    }

    pub(crate) fn release_effect(&mut self, _operation: &CapabilityOperation) {
        assert!(
            self.active > 0,
            "filesystem effect release requires a live operation"
        );
        assert!(
            self.outstanding_effects > 0,
            "filesystem effect registry count underflowed"
        );
        self.outstanding_effects -= 1;
    }
}

pub(crate) struct CapabilityOperation {
    pub(crate) authority: Arc<CapabilityAuthority>,
    pub(crate) windowed: bool,
    pub(crate) remembers_files: bool,
    /// Counted in this thread's window as a live effect, which may rebind files.
    pub(crate) window_effect: bool,
}

impl Drop for CapabilityOperation {
    fn drop(&mut self) {
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(
            state.active > 0,
            "filesystem capability operation count underflowed"
        );
        state.active -= 1;
        drop(state);
        if self.window_effect {
            let key = Arc::as_ptr(&self.authority) as usize;
            let _ = OBSERVATION_WINDOWS.try_with(|windows| {
                if let Some(window) = windows
                    .borrow_mut()
                    .iter_mut()
                    .find(|window| window.authority == key)
                {
                    window.effects = window.effects.saturating_sub(1);
                }
            });
        }
    }
}

impl StageToken {
    pub(crate) fn update(&self, phase: StageRegistryPhase) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .update_stage(self.id, phase)
    }

    pub(crate) fn prepare_promotion(
        &self,
        destination: &Directory,
        name: &LeafName,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
        displaced_park: Option<&FileParkRegistryToken>,
        expected_recovery: Option<&RecoveryRecord>,
    ) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .prepare_stage_promotion(
                self.id,
                destination.clone(),
                name.clone(),
                attempt_id,
                receipt,
                displaced_park,
                expected_recovery,
            )
    }

    pub(crate) fn allocate_publication_attempt(&self) -> io::Result<u64> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .allocate_stage_publication_attempt(self.id)
    }

    pub(crate) fn record_publication(
        &self,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
    ) {
        let Some(authority) = self.authority.upgrade() else {
            std::process::abort();
        };
        authority.record_stage_publication(self.id, attempt_id, receipt);
    }

    pub(crate) fn validate_publication_attempt(
        &self,
        attempt_id: u64,
        receipt: &platform::PublicationReceipt,
    ) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .validate_stage_publication(self.id, attempt_id, receipt)
    }

    pub(crate) fn discard(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .cleanup_stage(self.id)?;
        self.armed = false;
        Ok(())
    }

    pub(crate) fn disarm(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let operation = authority.enter()?;
        authority.disarm_stage(self.id, &operation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for StageToken {
    fn drop(&mut self) {
        if self.armed && self.discard().is_err() {
            if let Some(authority) = self.authority.upgrade() {
                authority.abandon_stage(self.id);
            }
            self.armed = false;
        }
    }
}

impl Drop for StageCreateToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_stage_create(self.id);
        }
    }
}

impl Drop for DirectoryCreateEffectToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_directory_create(self.id);
        }
    }
}

impl Drop for FileParkRegistryToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_file_park(self.id);
        }
    }
}

impl Drop for DirectoryParkRegistryToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_directory_park(self.id);
        }
    }
}
