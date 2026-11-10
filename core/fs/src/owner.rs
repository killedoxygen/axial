use super::*;

#[derive(Clone)]
pub struct EffectOwner {
    pub(crate) state: Arc<EffectOwnerState>,
}

impl_redacted_debug!(EffectOwner);

#[must_use = "refused effect retention returns its linear carrier"]
pub struct EffectOwnerRetentionError<T> {
    pub(crate) error: Option<io::Error>,
    pub(crate) carrier: Option<Box<T>>,
}

impl<T> fmt::Debug for EffectOwnerRetentionError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectOwnerRetentionError")
            .finish_non_exhaustive()
    }
}

impl<T> EffectOwnerRetentionError<T> {
    pub(crate) fn new(error: io::Error, carrier: T) -> Self {
        Self {
            error: Some(error),
            carrier: Some(Box::new(carrier)),
        }
    }

    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("effect retention failure retains its error")
    }

    pub fn into_parts(mut self) -> (io::Error, T) {
        (
            self.error
                .take()
                .expect("effect retention failure retains its error"),
            self.carrier
                .take()
                .map(|carrier| *carrier)
                .expect("effect retention failure retains its carrier"),
        )
    }
}

impl<T> Drop for EffectOwnerRetentionError<T> {
    fn drop(&mut self) {
        if self.error.is_some() || self.carrier.is_some() {
            std::process::abort();
        }
    }
}

pub(crate) struct EffectOwnerState {
    pub(crate) id: u64,
    pub(crate) authority: Weak<CapabilityAuthority>,
    pub(crate) anchor: Directory,
    pub(crate) effects: Mutex<EffectOwnerRecords>,
    #[cfg(test)]
    pub(crate) settlement_pause: Mutex<Option<EffectOwnerSettlementPause>>,
}

#[cfg(test)]
pub(crate) struct EffectOwnerSettlementPause {
    pub(crate) extracted: Arc<std::sync::Barrier>,
    pub(crate) resume: Arc<std::sync::Barrier>,
}

pub(crate) struct EffectOwnerRecords {
    pub(crate) next_id: u64,
    pub(crate) settling: bool,
    pub(crate) in_flight: usize,
    pub(crate) effects: BTreeMap<u64, OwnedEffect>,
}

pub(crate) type ReceiptLiveness = Arc<AtomicBool>;

pub(crate) fn receipt_is_live(live: &ReceiptLiveness) -> bool {
    // Receipt Drop publishes abandonment even while settlement owns the extracted record.
    live.load(Ordering::Acquire)
}

pub(crate) enum OwnedEffect {
    StageCreateCleanup(FileCreateObligation),
    DirectoryCreateCompletion(DirectoryCreateObligation),
    DirectoryCreatePreservation(DirectoryCreatePreservation),
    StageDiscard(StageDiscardObligation),
    FileParkRemoval(FileParkObligation),
    ParkedFileRemoval(ParkedFile),
    FileRemoval(FileRemovalObligation),
    DirectoryParkRemoval(DirectoryParkObligation),
    ParkedDirectoryRemoval(ParkedDirectory),
    DirectoryRemoval(DirectoryRemovalObligation),
    ParkedDirectoryTreeRemoval(RetainedDirectoryTreeRemoval),
    DirectoryTreeRemoval(DirectoryTreeRemovalObligation),
    FilePromotion(OwnedReceipt<Box<FilePromotionObligation>>),
    FileReplace(Box<OwnedReceipt<FileReplaceObligation>>),
    FileMove(OwnedReceipt<FileMoveObligation>),
    DirectoryMove(OwnedReceipt<DirectoryMoveObligation>),
}

/// An effect an owner reconciles until a live receipt claims its terminal result.
pub(crate) trait ReceiptEffect: Sized {
    type Applied;
    type NoEffect;

    fn reconcile_owned(self) -> Resolution<Self::Applied, Self::NoEffect, Self>;
    fn wrap(owned: OwnedReceipt<Self>) -> OwnedEffect;
    fn project(effect: OwnedEffect) -> Result<OwnedReceipt<Self>, OwnedEffect>;
}

macro_rules! impl_owned_receipt {
    ($effect:ty, $outcome:ty) => {
        impl Receipt<$effect> {
            pub fn claim(self) -> $outcome {
                claim_receipt(self)
            }
        }
    };
}

impl ReceiptEffect for Box<FilePromotionObligation> {
    type Applied = FileCapability;
    type NoEffect = SealedStagedFile;

    fn reconcile_owned(self) -> FilePromotionResolution {
        (*self).reconcile()
    }

    fn wrap(owned: OwnedReceipt<Self>) -> OwnedEffect {
        OwnedEffect::FilePromotion(owned)
    }

    fn project(effect: OwnedEffect) -> Result<OwnedReceipt<Self>, OwnedEffect> {
        match effect {
            OwnedEffect::FilePromotion(owned) => Ok(owned),
            effect => Err(effect),
        }
    }
}

impl ReceiptEffect for FileReplaceObligation {
    type Applied = (FileCapability, Option<ParkedFile>);
    type NoEffect = (SealedStagedFile, ReplaceDestination);

    fn reconcile_owned(self) -> Resolution<Self::Applied, Self::NoEffect, Self> {
        match self.reconcile() {
            FileReplaceResolution::Replaced { current, displaced } => {
                Resolution::Applied((current, displaced))
            }
            FileReplaceResolution::NoEffect {
                staged,
                destination,
            } => Resolution::NoEffect((staged, destination)),
            FileReplaceResolution::Indeterminate(obligation) => {
                Resolution::Indeterminate(obligation)
            }
        }
    }

    fn wrap(owned: OwnedReceipt<Self>) -> OwnedEffect {
        OwnedEffect::FileReplace(Box::new(owned))
    }

    fn project(effect: OwnedEffect) -> Result<OwnedReceipt<Self>, OwnedEffect> {
        match effect {
            OwnedEffect::FileReplace(owned) => Ok(*owned),
            effect => Err(effect),
        }
    }
}

impl ReceiptEffect for FileMoveObligation {
    type Applied = FileCapability;
    type NoEffect = FileCapability;

    fn reconcile_owned(self) -> FileMoveResolution {
        self.reconcile()
    }

    fn wrap(owned: OwnedReceipt<Self>) -> OwnedEffect {
        OwnedEffect::FileMove(owned)
    }

    fn project(effect: OwnedEffect) -> Result<OwnedReceipt<Self>, OwnedEffect> {
        match effect {
            OwnedEffect::FileMove(owned) => Ok(owned),
            effect => Err(effect),
        }
    }
}

impl ReceiptEffect for DirectoryMoveObligation {
    type Applied = Directory;
    type NoEffect = Directory;

    fn reconcile_owned(self) -> DirectoryMoveResolution {
        self.reconcile()
    }

    fn wrap(owned: OwnedReceipt<Self>) -> OwnedEffect {
        OwnedEffect::DirectoryMove(owned)
    }

    fn project(effect: OwnedEffect) -> Result<OwnedReceipt<Self>, OwnedEffect> {
        match effect {
            OwnedEffect::DirectoryMove(owned) => Ok(owned),
            effect => Err(effect),
        }
    }
}

pub(crate) enum Owned<E: ReceiptEffect> {
    Pending(E),
    Applied(E::Applied),
    NoEffect(E::NoEffect),
}

/// An owner-held effect and the liveness of the receipt that may claim it.
pub(crate) struct OwnedReceipt<E: ReceiptEffect> {
    pub(crate) state: Owned<E>,
    pub(crate) live: ReceiptLiveness,
}

impl<E: ReceiptEffect> OwnedReceipt<E> {
    pub(crate) fn is_pending(&self) -> bool {
        matches!(self.state, Owned::Pending(_))
    }

    pub(crate) fn is_domain_pending(&self) -> bool {
        self.is_pending() || receipt_is_live(&self.live)
    }

    /// Reconciles a pending effect; a settled result whose receipt was dropped is
    /// abandoned instead of retained.
    pub(crate) fn settle(self) -> Option<Self> {
        let state = match self.state {
            Owned::Pending(effect) => match effect.reconcile_owned() {
                Resolution::Applied(applied) => Owned::Applied(applied),
                Resolution::NoEffect(item) => Owned::NoEffect(item),
                Resolution::Indeterminate(effect) => Owned::Pending(effect),
            },
            settled => settled,
        };
        let owned = Self {
            state,
            live: self.live,
        };
        (owned.is_pending() || receipt_is_live(&owned.live)).then_some(owned)
    }
}

/// Claims an owner-settled effect. Dropping the receipt abandons its result.
#[must_use = "effect receipts must be claimed after explicit owner settlement"]
pub struct Receipt<E> {
    pub(crate) owner: Arc<EffectOwnerState>,
    pub(crate) id: u64,
    pub(crate) live: ReceiptLiveness,
    pub(crate) effect: std::marker::PhantomData<fn() -> E>,
}

impl<E> fmt::Debug for Receipt<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Receipt").finish_non_exhaustive()
    }
}

impl<E> Drop for Receipt<E> {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

#[must_use = "receipt outcomes must be handled"]
#[derive(Debug)]
pub enum ReceiptOutcome<A, N, R> {
    Pending(R),
    Applied(A),
    NoEffect(N),
}

pub type FilePromotionReceipt = Receipt<Box<FilePromotionObligation>>;

pub type FileReplaceReceipt = Receipt<FileReplaceObligation>;

pub type FileMoveReceipt = Receipt<FileMoveObligation>;

pub type DirectoryMoveReceipt = Receipt<DirectoryMoveObligation>;

pub type FilePromotionReceiptOutcome =
    ReceiptOutcome<FileCapability, SealedStagedFile, FilePromotionReceipt>;

pub type FileReplaceReceiptOutcome = ReceiptOutcome<
    (FileCapability, Option<ParkedFile>),
    (SealedStagedFile, ReplaceDestination),
    FileReplaceReceipt,
>;

pub type FileMoveReceiptOutcome = ReceiptOutcome<FileCapability, FileCapability, FileMoveReceipt>;

pub type DirectoryMoveReceiptOutcome = ReceiptOutcome<Directory, Directory, DirectoryMoveReceipt>;

impl_owned_receipt!(Box<FilePromotionObligation>, FilePromotionReceiptOutcome);

impl_owned_receipt!(FileReplaceObligation, FileReplaceReceiptOutcome);

impl_owned_receipt!(FileMoveObligation, FileMoveReceiptOutcome);

impl_owned_receipt!(DirectoryMoveObligation, DirectoryMoveReceiptOutcome);

pub(crate) fn claim_receipt<E: ReceiptEffect>(
    receipt: Receipt<E>,
) -> ReceiptOutcome<E::Applied, E::NoEffect, Receipt<E>> {
    let mut records = receipt
        .owner
        .effects
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(effect) = records.effects.remove(&receipt.id) else {
        drop(records);
        return ReceiptOutcome::Pending(receipt);
    };
    let owned = match E::project(effect) {
        Ok(owned)
            if Arc::ptr_eq(&owned.live, &receipt.live)
                && receipt_is_live(&owned.live)
                && !owned.is_pending() =>
        {
            owned
        }
        Ok(owned) => {
            records.effects.insert(receipt.id, E::wrap(owned));
            drop(records);
            return ReceiptOutcome::Pending(receipt);
        }
        Err(effect) => {
            records.effects.insert(receipt.id, effect);
            drop(records);
            return ReceiptOutcome::Pending(receipt);
        }
    };
    drop(records);
    deactivate_claimed_owner(&receipt.owner);
    match owned.state {
        Owned::Applied(applied) => ReceiptOutcome::Applied(applied),
        Owned::NoEffect(item) => ReceiptOutcome::NoEffect(item),
        Owned::Pending(_) => unreachable!("claimed receipt is settled"),
    }
}

impl EffectOwner {
    pub fn anchor_identity(&self) -> DirectoryIdentity {
        self.state.anchor.inner.identity
    }

    pub fn has_pending(&self) -> bool {
        let records = self
            .state
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records.settling || records.in_flight != 0 || !records.effects.is_empty()
    }

    pub fn require_settled(&self) -> io::Result<()> {
        if self.has_pending() {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect owner has unsettled or unclaimed effects",
            ))
        } else {
            Ok(())
        }
    }

    pub fn settle(&self) -> io::Result<()> {
        self.state.settle(false)
    }

    pub fn retain_stage_create_cleanup(
        &self,
        obligation: FileCreateObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileCreateObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                authority.stage_create_is_within(&obligation.token, anchor)
            },
            OwnedEffect::StageCreateCleanup,
        )
        .map(|_| ())
    }

    pub fn retain_directory_create_completion(
        &self,
        obligation: DirectoryCreateObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryCreateObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                authority.directory_create_is_within(&obligation.token, anchor)
            },
            OwnedEffect::DirectoryCreateCompletion,
        )
        .map(|_| ())
    }

    pub fn retain_directory_create_preservation(
        &self,
        preservation: DirectoryCreatePreservation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryCreatePreservation>> {
        self.retain(
            preservation,
            |authority, anchor, preservation| {
                authority.directory_create_is_within(&preservation.token, anchor)
            },
            OwnedEffect::DirectoryCreatePreservation,
        )
        .map(|_| ())
    }

    pub fn retain_stage_discard(
        &self,
        obligation: StageDiscardObligation,
    ) -> Result<(), EffectOwnerRetentionError<StageDiscardObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                obligation
                    .token
                    .as_ref()
                    .is_some_and(|token| authority.stage_is_within(token, anchor))
            },
            OwnedEffect::StageDiscard,
        )
        .map(|_| ())
    }

    pub fn retain_file_park_removal(
        &self,
        obligation: FileParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| file_park_obligation_is_within(obligation, anchor),
            OwnedEffect::FileParkRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_file_removal(
        &self,
        parked: ParkedFile,
    ) -> Result<(), EffectOwnerRetentionError<ParkedFile>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedFileRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_file_removal(
        &self,
        obligation: FileRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::FileRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_park_removal(
        &self,
        obligation: DirectoryParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation.parent.is_within(anchor)
                    && obligation
                        .directory
                        .as_ref()
                        .is_some_and(|directory| directory.is_within(anchor))
            },
            OwnedEffect::DirectoryParkRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_directory_removal(
        &self,
        parked: ParkedDirectory,
    ) -> Result<(), EffectOwnerRetentionError<ParkedDirectory>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedDirectoryRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_removal(
        &self,
        obligation: DirectoryRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::DirectoryRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_directory_tree_removal(
        &self,
        retained: RetainedDirectoryTreeRemoval,
    ) -> Result<(), EffectOwnerRetentionError<RetainedDirectoryTreeRemoval>> {
        self.retain(
            retained,
            |_, anchor, retained| {
                retained
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::ParkedDirectoryTreeRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_tree_removal(
        &self,
        obligation: DirectoryTreeRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryTreeRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::DirectoryTreeRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_file_promotion(
        &self,
        obligation: Box<FilePromotionObligation>,
    ) -> Result<FilePromotionReceipt, EffectOwnerRetentionError<Box<FilePromotionObligation>>> {
        self.retain_receipt(obligation, |anchor, obligation| {
            obligation.retained.file.parent.is_within(anchor)
                && obligation.destination.is_within(anchor)
        })
    }

    pub fn retain_file_replace(
        &self,
        obligation: FileReplaceObligation,
    ) -> Result<FileReplaceReceipt, EffectOwnerRetentionError<FileReplaceObligation>> {
        self.retain_receipt(obligation, |anchor, obligation| {
            file_replace_obligation_is_within(obligation, anchor)
        })
    }

    pub fn retain_file_move(
        &self,
        obligation: FileMoveObligation,
    ) -> Result<FileMoveReceipt, EffectOwnerRetentionError<FileMoveObligation>> {
        self.retain_receipt(obligation, |anchor, obligation| {
            obligation
                .file
                .as_ref()
                .is_some_and(|file| file.parent.is_within(anchor))
                && obligation.destination.is_within(anchor)
        })
    }

    pub fn retain_directory_move(
        &self,
        obligation: DirectoryMoveObligation,
    ) -> Result<DirectoryMoveReceipt, EffectOwnerRetentionError<DirectoryMoveObligation>> {
        self.retain_receipt(obligation, |anchor, obligation| {
            obligation
                .directory
                .as_ref()
                .is_some_and(|directory| directory.is_within(anchor))
                && obligation.destination.is_within(anchor)
        })
    }

    pub(crate) fn retain_receipt<E: ReceiptEffect>(
        &self,
        effect: E,
        within: impl FnOnce(&Directory, &E) -> bool,
    ) -> Result<Receipt<E>, EffectOwnerRetentionError<E>> {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            effect,
            |_, anchor, effect| within(anchor, effect),
            move |effect| {
                E::wrap(OwnedReceipt {
                    state: Owned::Pending(effect),
                    live: record_live,
                })
            },
        )
        .map(|id| Receipt {
            owner: self.state.clone(),
            id,
            live,
            effect: std::marker::PhantomData,
        })
    }

    pub(crate) fn retain<T>(
        &self,
        carrier: T,
        validate: impl FnOnce(&Arc<CapabilityAuthority>, &Directory, &T) -> bool,
        wrap: impl FnOnce(T) -> OwnedEffect,
    ) -> Result<u64, EffectOwnerRetentionError<T>> {
        let Some(authority) = self.state.authority.upgrade() else {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        };
        let operation = match authority.enter_effect_retention(self.state.id) {
            Ok(operation) => operation,
            Err(error) => return Err(EffectOwnerRetentionError::new(error, carrier)),
        };
        if !validate(&authority, &self.state.anchor, &carrier) {
            return Err(EffectOwnerRetentionError::new(
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "filesystem effect lies outside its owner's anchored subtree",
                ),
                carrier,
            ));
        }
        authority.retain_effect_owner_record(&self.state, &operation, carrier, wrap)
    }
}

pub(crate) fn file_park_obligation_is_within(
    obligation: &FileParkObligation,
    anchor: &Directory,
) -> bool {
    obligation
        .request
        .as_ref()
        .is_some_and(|request| request.file.parent.is_within(anchor))
}

pub(crate) fn replace_destination_is_within(
    destination: &ReplaceDestination,
    anchor: &Directory,
) -> bool {
    match destination {
        ReplaceDestination::Vacant { parent, .. } => parent.is_within(anchor),
        ReplaceDestination::Existing(request) => request.file.parent.is_within(anchor),
        ReplaceDestination::Preserved(file) => file.parent.is_within(anchor),
    }
}

pub(crate) fn file_promotion_obligation_is_within(
    obligation: &FilePromotionObligation,
    anchor: &Directory,
) -> bool {
    obligation.retained.file.parent.is_within(anchor) && obligation.destination.is_within(anchor)
}

pub(crate) fn file_replace_obligation_is_within(
    obligation: &FileReplaceObligation,
    anchor: &Directory,
) -> bool {
    obligation
        .state
        .as_ref()
        .is_some_and(|state| match state.as_ref() {
            FileReplaceObligationState::Parking { park, staged, .. } => {
                file_park_obligation_is_within(park, anchor) && staged.file.parent.is_within(anchor)
            }
            FileReplaceObligationState::Promoting {
                promotion,
                displaced,
                fallback,
                ..
            } => {
                file_promotion_obligation_is_within(promotion, anchor)
                    && displaced
                        .as_ref()
                        .is_none_or(|parked| parked.parent.is_within(anchor))
                    && replace_destination_is_within(fallback, anchor)
            }
            FileReplaceObligationState::RestoreParked { parked, staged, .. } => {
                parked.parent.is_within(anchor) && staged.file.parent.is_within(anchor)
            }
            FileReplaceObligationState::RestoreObligation {
                restore, staged, ..
            } => {
                restore
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
                    && staged.file.parent.is_within(anchor)
            }
        })
}

impl EffectOwnerState {
    pub(crate) fn settle(self: &Arc<Self>, terminal: bool) -> io::Result<()> {
        {
            let records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if records.settling || records.in_flight != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect owner settlement is already in progress",
                ));
            }
            if records.effects.is_empty() {
                return Ok(());
            }
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let _terminal_scope = if terminal {
            Some(TerminalEffectSettlementScope::begin(&authority, self.id)?)
        } else {
            None
        };
        let operation = authority.enter_effect_settlement(self.id, terminal)?;
        let (pending, in_flight) = {
            let mut records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if records.settling {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect owner settlement is already in progress",
                ));
            }
            if records.effects.is_empty() {
                return Ok(());
            }
            let in_flight = records.effects.len();
            records.settling = true;
            records.in_flight = in_flight;
            (std::mem::take(&mut records.effects), in_flight)
        };
        #[cfg(test)]
        self.pause_after_settlement_extraction();
        let mut settled = BTreeMap::new();
        let mut blocked = false;
        for (id, effect) in pending {
            if blocked {
                settled.insert(id, effect);
                continue;
            }
            if let Some(effect) = effect.settle() {
                blocked = effect.is_unresolved();
                settled.insert(id, effect);
            }
        }
        {
            let mut records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(records.settling);
            assert_eq!(records.in_flight, in_flight);
            for (id, effect) in settled {
                assert!(records.effects.insert(id, effect).is_none());
            }
            records.in_flight = 0;
            records.settling = false;
        }
        drop(_terminal_scope);
        drop(operation);
        authority.deactivate_effect_owner_if_empty(self);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pause_after_settlement_extraction(&self) {
        let pause = self
            .settlement_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pause) = pause {
            pause.extracted.wait();
            pause.resume.wait();
        }
    }

    pub(crate) fn has_domain_pending(&self) -> bool {
        self.effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effects
            .values()
            .any(OwnedEffect::is_domain_pending)
    }

    pub(crate) fn take_for_terminal_disposal(&self) -> Vec<OwnedEffect> {
        let mut records = self
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!records.settling && records.in_flight == 0);
        std::mem::take(&mut records.effects).into_values().collect()
    }
}

impl OwnedEffect {
    pub(crate) fn is_domain_pending(&self) -> bool {
        match self {
            Self::DirectoryCreateCompletion(_)
            | Self::DirectoryCreatePreservation(_)
            | Self::ParkedDirectoryTreeRemoval(_)
            | Self::DirectoryTreeRemoval(_) => true,
            Self::FilePromotion(owned) => owned.is_domain_pending(),
            Self::FileReplace(owned) => owned.is_domain_pending(),
            Self::FileMove(owned) => owned.is_domain_pending(),
            Self::DirectoryMove(owned) => owned.is_domain_pending(),
            _ => false,
        }
    }

    pub(crate) fn is_unresolved(&self) -> bool {
        match self {
            Self::FilePromotion(owned) => owned.is_pending(),
            Self::FileReplace(owned) => owned.is_pending(),
            Self::FileMove(owned) => owned.is_pending(),
            Self::DirectoryMove(owned) => owned.is_pending(),
            _ => true,
        }
    }

    pub(crate) fn settle(self) -> Option<Self> {
        match self {
            Self::StageCreateCleanup(obligation) => match obligation.reconcile() {
                FileCreateResolution::Created(staged) => owned_stage_discard(staged.discard()),
                FileCreateResolution::NoEffect(_) => None,
                FileCreateResolution::Indeterminate(obligation) => {
                    Some(Self::StageCreateCleanup(obligation))
                }
            },
            Self::DirectoryCreateCompletion(obligation) => match obligation.reconcile() {
                DirectoryCreateResolution::Created(directory) => {
                    drop(directory);
                    None
                }
                DirectoryCreateResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryCreateCompletion(obligation))
                }
            },
            Self::DirectoryCreatePreservation(preservation) => preservation
                .acknowledge_preserved()
                .err()
                .map(Self::DirectoryCreatePreservation),
            Self::StageDiscard(obligation) => match obligation.reconcile() {
                StageDiscardResolution::Discarded => None,
                StageDiscardResolution::Indeterminate(obligation) => {
                    Some(Self::StageDiscard(obligation))
                }
            },
            Self::FileParkRemoval(obligation) => match obligation.reconcile() {
                FileParkResolution::Parked(parked) => owned_parked_file_removal(parked),
                FileParkResolution::NoEffect(request) => {
                    drop(request);
                    None
                }
                FileParkResolution::Preserved { file, .. } => {
                    drop(file);
                    None
                }
                FileParkResolution::Indeterminate(obligation) => {
                    Some(Self::FileParkRemoval(obligation))
                }
            },
            Self::ParkedFileRemoval(parked) => owned_parked_file_removal(parked),
            Self::FileRemoval(obligation) => match obligation.reconcile() {
                FileRemovalResolution::Applied(()) => None,
                FileRemovalResolution::NoEffect(parked) => owned_parked_file_removal(parked),
                FileRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::FileRemoval(obligation))
                }
            },
            Self::DirectoryParkRemoval(obligation) => match obligation.reconcile() {
                DirectoryParkResolution::Applied(parked) => owned_parked_directory_removal(parked),
                DirectoryParkResolution::NoEffect(directory) => {
                    drop(directory);
                    None
                }
                DirectoryParkResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryParkRemoval(obligation))
                }
            },
            Self::ParkedDirectoryRemoval(parked) => owned_parked_directory_removal(parked),
            Self::DirectoryRemoval(obligation) => match obligation.reconcile() {
                DirectoryRemovalResolution::Applied(()) => None,
                DirectoryRemovalResolution::NoEffect(parked) => {
                    owned_parked_directory_removal(parked)
                }
                DirectoryRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryRemoval(obligation))
                }
            },
            Self::ParkedDirectoryTreeRemoval(parked) => owned_parked_directory_tree_removal(parked),
            Self::DirectoryTreeRemoval(obligation) => match obligation.reconcile() {
                DirectoryTreeRemovalResolution::Removed => None,
                DirectoryTreeRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryTreeRemoval(obligation))
                }
            },
            Self::FilePromotion(owned) => owned.settle().map(Self::FilePromotion),
            Self::FileReplace(owned) => (*owned)
                .settle()
                .map(|owned| Self::FileReplace(Box::new(owned))),
            Self::FileMove(owned) => owned.settle().map(Self::FileMove),
            Self::DirectoryMove(owned) => owned.settle().map(Self::DirectoryMove),
        }
    }
}

pub(crate) fn owned_stage_discard(outcome: StageDiscardOutcome) -> Option<OwnedEffect> {
    match outcome {
        StageDiscardOutcome::Discarded => None,
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::StageDiscard(obligation))
        }
    }
}

pub(crate) fn owned_parked_file_removal(parked: ParkedFile) -> Option<OwnedEffect> {
    match parked.remove() {
        FileRemovalOutcome::Applied(()) => None,
        FileRemovalOutcome::NoEffect { item: parked, .. } => {
            Some(OwnedEffect::ParkedFileRemoval(parked))
        }
        FileRemovalOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::FileRemoval(obligation))
        }
    }
}

pub(crate) fn owned_parked_directory_removal(parked: ParkedDirectory) -> Option<OwnedEffect> {
    match parked.remove_empty() {
        DirectoryRemovalOutcome::Applied(()) => None,
        DirectoryRemovalOutcome::NoEffect { item: parked, .. } => {
            Some(OwnedEffect::ParkedDirectoryRemoval(parked))
        }
        DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::DirectoryRemoval(obligation))
        }
    }
}

pub(crate) fn owned_parked_directory_tree_removal(
    retained: RetainedDirectoryTreeRemoval,
) -> Option<OwnedEffect> {
    match retained.retry() {
        DirectoryTreeRemovalOutcome::Removed => None,
        DirectoryTreeRemovalOutcome::Retained { retained, .. } => {
            Some(OwnedEffect::ParkedDirectoryTreeRemoval(retained))
        }
        DirectoryTreeRemovalOutcome::Indeterminate(obligation) => {
            Some(OwnedEffect::DirectoryTreeRemoval(obligation))
        }
    }
}

pub(crate) fn deactivate_claimed_owner(owner: &Arc<EffectOwnerState>) {
    if let Some(authority) = owner.authority.upgrade() {
        authority.deactivate_effect_owner_if_empty(owner);
    }
}
