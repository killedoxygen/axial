use crate::platform;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::io;
use unicode_casefold::UnicodeCaseFold as _;
use unicode_normalization::UnicodeNormalization as _;

const RECOVERY_SLOT_COUNT: usize = 64;
const RECOVERY_FRAMES_PER_SLOT: usize = 2;
const RECOVERY_FRAME_BYTES: usize = 16 * 1024;
const RECOVERY_REGION_BYTES: u64 =
    RECOVERY_SLOT_COUNT as u64 * RECOVERY_FRAMES_PER_SLOT as u64 * RECOVERY_FRAME_BYTES as u64;
const SUCCESSOR_AGGREGATE_SLOT_COUNT: usize = 64;
pub(crate) const RECOVERY_CONTROL_BYTES: u64 = RECOVERY_REGION_BYTES
    + SUCCESSOR_AGGREGATE_SLOT_COUNT as u64
        * RECOVERY_FRAMES_PER_SLOT as u64
        * RECOVERY_FRAME_BYTES as u64;

const MAGIC: &[u8; 8] = b"AXRECV01";
const SCHEMA: u16 = 1;
const HEADER_BYTES: usize = 52;
const CHECKSUM_BYTES: usize = 32;
const FRAME_BODY_BYTES: usize = RECOVERY_FRAME_BYTES - CHECKSUM_BYTES;
const MAX_COMPONENTS: usize = 32;
const MAX_NAME_BYTES: usize = 255;
const MAX_NAME_UTF16_UNITS: usize = 255;
const MAX_RECORD_PAYLOAD_BYTES: usize = 24 + MAX_COMPONENTS * (2 + MAX_NAME_BYTES) + 2 * 40;
pub(crate) const MAX_RECOVERABLE_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_LIVE_PROOF_BYTES: u64 = 128 * 1024 * 1024;
const CHECKSUM_DOMAIN: &[u8] = b"axial.fs.recovery-frame.v1\0";
const ROOT_LEASE_NAME: &str = ".axial-root.lease";
const RECOVERY_STAGE_PREFIX: &str = ".axial-rstage-";
const RECOVERY_PARK_PREFIX: &str = ".axial-rpark-";

const _: () = assert!(HEADER_BYTES + MAX_RECORD_PAYLOAD_BYTES <= FRAME_BODY_BYTES);

#[cfg(target_os = "linux")]
const PLATFORM_TAG: u8 = 1;
#[cfg(target_os = "macos")]
const PLATFORM_TAG: u8 = 2;
#[cfg(target_os = "windows")]
const PLATFORM_TAG: u8 = 3;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("recovery frames require a supported Linux, macOS, or Windows target");

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("recovery frame is invalid")]
pub(crate) struct RecoveryCodecError;

type Result<T> = std::result::Result<T, RecoveryCodecError>;

impl From<RecoveryCodecError> for io::Error {
    fn from(_: RecoveryCodecError) -> Self {
        codec_io_error()
    }
}

fn codec_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "root recovery control is invalid",
    )
}

fn random_nonzero_id() -> [u8; 16] {
    let mut value = [0; 16];
    while value == [0; 16] {
        rand::rngs::OsRng.fill_bytes(&mut value)
    }
    value
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecoveryFrameAddress {
    lane_nonce: [u8; 16],
    slot: u8,
    frame: u8,
}

impl RecoveryFrameAddress {
    pub(crate) fn new(lane_nonce: [u8; 16], slot: u8, frame: u8) -> Result<Self> {
        let address = Self {
            lane_nonce,
            slot,
            frame,
        };
        address.validate()?;
        Ok(address)
    }

    fn validate(self) -> Result<()> {
        if self.lane_nonce == [0; 16]
            || usize::from(self.slot) >= RECOVERY_SLOT_COUNT
            || usize::from(self.frame) >= RECOVERY_FRAMES_PER_SLOT
        {
            return Err(RecoveryCodecError);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryName(String);

impl RecoveryName {
    pub(crate) fn new_exact(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_name(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum RecoveryPhase {
    StagePrepared = 1,
    StageSealed = 2,
    ReplacePrepared = 3,
    PublishPrepared = 4,
    RemovePrepared = 5,
    RemoveCommitted = 6,
}

impl RecoveryPhase {
    fn decode(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::StagePrepared),
            2 => Ok(Self::StageSealed),
            3 => Ok(Self::ReplacePrepared),
            4 => Ok(Self::PublishPrepared),
            5 => Ok(Self::RemovePrepared),
            6 => Ok(Self::RemoveCommitted),
            _ => Err(RecoveryCodecError),
        }
    }

    pub(crate) fn owns_target(self) -> bool {
        matches!(
            self,
            Self::StageSealed
                | Self::ReplacePrepared
                | Self::PublishPrepared
                | Self::RemoveCommitted
        )
    }

    pub(crate) fn owns_park(self) -> bool {
        !matches!(self, Self::RemovePrepared)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplacementCarrier {
    Unobserved,
    Absent,
    Unsealed,
    Other,
    Old,
    New,
    OldAndNew,
}

impl ReplacementCarrier {
    fn is_observed(self) -> bool {
        self != Self::Unobserved
    }

    fn matches_old(self) -> bool {
        matches!(self, Self::Old | Self::OldAndNew)
    }

    fn matches_new(self) -> bool {
        matches!(self, Self::New | Self::OldAndNew)
    }

    fn has_valid_proof_role(self, proofs_equal: bool) -> bool {
        match self {
            Self::OldAndNew => proofs_equal,
            Self::Old | Self::New => !proofs_equal,
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplacementTopology {
    proofs_equal: bool,
    stage: ReplacementCarrier,
    target: ReplacementCarrier,
    park: ReplacementCarrier,
}

impl ReplacementTopology {
    fn from_record(
        record: &RecoveryRecord,
        stage: ReplacementCarrier,
        target: ReplacementCarrier,
        park: ReplacementCarrier,
    ) -> Option<Self> {
        record.validate().ok()?;
        let old = record.old?;
        Some(Self {
            proofs_equal: record.new == Some(old),
            stage,
            target,
            park,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplacementAction {
    Advance(RecoveryPhase),
    RemoveStage,
    ParkTarget,
    PublishStage,
    RestorePark,
    RemovePark,
    NoEffect,
    Applied,
}

pub(crate) fn classify_replacement(
    record: &RecoveryRecord,
    (stage, target, park): (ReplacementCarrier, ReplacementCarrier, ReplacementCarrier),
) -> Option<ReplacementAction> {
    use RecoveryPhase as Phase;
    use ReplacementAction as Action;
    use ReplacementCarrier as Carrier;

    let topology = ReplacementTopology::from_record(record, stage, target, park)?;
    let phase = record.phase;
    if topology.stage == Carrier::Unobserved
        || topology.target.is_observed() != phase.owns_target()
        || topology.park.is_observed() != phase.owns_park()
        || [topology.stage, topology.target, topology.park]
            .into_iter()
            .any(|carrier| !carrier.has_valid_proof_role(topology.proofs_equal))
    {
        return None;
    }

    let stage_new = topology.stage.matches_new();
    let target_old = topology.target.matches_old();
    let target_new = topology.target.matches_new();
    let park_old = topology.park.matches_old();
    match phase {
        Phase::StagePrepared => match topology.stage {
            Carrier::Unsealed if topology.park == Carrier::Absent => Some(Action::RemoveStage),
            Carrier::Absent if topology.park == Carrier::Absent => Some(Action::NoEffect),
            _ => None,
        },
        Phase::StageSealed if stage_new && topology.park == Carrier::Absent => {
            if target_old {
                Some(Action::Advance(Phase::ReplacePrepared))
            } else if matches!(
                topology.target,
                Carrier::Absent | Carrier::Other | Carrier::New
            ) {
                Some(Action::Advance(Phase::RemovePrepared))
            } else {
                None
            }
        }
        Phase::StageSealed => None,
        Phase::ReplacePrepared => {
            if stage_new && target_old && topology.park == Carrier::Absent {
                Some(Action::ParkTarget)
            } else if stage_new && topology.target == Carrier::Absent && park_old {
                Some(Action::Advance(Phase::PublishPrepared))
            } else if stage_new
                && topology.park == Carrier::Absent
                && matches!(
                    topology.target,
                    Carrier::Absent | Carrier::Other | Carrier::New
                )
            {
                Some(Action::Advance(Phase::RemovePrepared))
            } else if topology.stage == Carrier::Absent
                && target_old
                && topology.park == Carrier::Absent
            {
                Some(Action::NoEffect)
            } else if topology.stage == Carrier::Absent
                && topology.target == Carrier::Absent
                && park_old
            {
                Some(Action::RestorePark)
            } else {
                None
            }
        }
        Phase::PublishPrepared => {
            if stage_new && topology.target == Carrier::Absent && park_old {
                Some(Action::PublishStage)
            } else if topology.stage == Carrier::Absent && target_new && park_old {
                Some(Action::RemovePark)
            } else if topology.stage == Carrier::Absent
                && target_new
                && topology.park == Carrier::Absent
            {
                Some(Action::Advance(Phase::RemoveCommitted))
            } else if stage_new && target_old && topology.park == Carrier::Absent {
                Some(Action::Advance(Phase::RemovePrepared))
            } else if topology.stage == Carrier::Absent
                && topology.target == Carrier::Old
                && topology.park == Carrier::Absent
            {
                Some(Action::NoEffect)
            } else if topology.stage == Carrier::Absent
                && topology.target == Carrier::Absent
                && park_old
            {
                Some(Action::RestorePark)
            } else {
                None
            }
        }
        Phase::RemovePrepared => match topology.stage {
            Carrier::New | Carrier::OldAndNew => Some(Action::RemoveStage),
            Carrier::Absent => Some(Action::NoEffect),
            _ => None,
        },
        Phase::RemoveCommitted => {
            if stage_new && target_old && topology.park == Carrier::Absent {
                Some(Action::ParkTarget)
            } else if stage_new
                && topology.target == Carrier::Absent
                && (topology.park == Carrier::Absent || park_old)
            {
                Some(Action::PublishStage)
            } else if topology.stage == Carrier::Absent && target_new && park_old {
                Some(Action::RemovePark)
            } else if topology.stage == Carrier::Absent
                && target_new
                && topology.park == Carrier::Absent
            {
                Some(Action::Applied)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SYNC_BEFORE_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) struct RecoverySyncFailureTestGuard {
    thread: std::thread::ThreadId,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
impl Drop for RecoverySyncFailureTestGuard {
    fn drop(&mut self) {
        assert_eq!(
            self.thread,
            std::thread::current().id(),
            "recovery sync test hook guard changed threads"
        );
        FAIL_NEXT_SYNC_BEFORE_BARRIER.set(false);
    }
}

#[cfg(test)]
pub(crate) fn install_pre_barrier_sync_failure() -> RecoverySyncFailureTestGuard {
    FAIL_NEXT_SYNC_BEFORE_BARRIER.with(|slot| {
        assert!(
            !slot.replace(true),
            "recovery sync test hook is already installed"
        );
    });
    RecoverySyncFailureTestGuard {
        thread: std::thread::current().id(),
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(test)]
fn take_pre_barrier_sync_failure() -> bool {
    FAIL_NEXT_SYNC_BEFORE_BARRIER.replace(false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryFileProof {
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryRecord {
    pub(crate) operation_id: [u8; 16],
    pub(crate) phase: RecoveryPhase,
    pub(crate) destination_parent: Vec<RecoveryName>,
    pub(crate) destination_leaf: RecoveryName,
    pub(crate) old: Option<RecoveryFileProof>,
    pub(crate) new: Option<RecoveryFileProof>,
}

impl RecoveryRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.operation_id == [0; 16]
            || self
                .destination_parent
                .len()
                .checked_add(1)
                .ok_or(RecoveryCodecError)?
                > MAX_COMPONENTS
        {
            return Err(RecoveryCodecError);
        }
        for name in self
            .destination_parent
            .iter()
            .chain([&self.destination_leaf])
        {
            validate_name(name.as_str())?;
        }
        if self.destination_parent.is_empty()
            && portable_name_key(&self.destination_leaf) == portable_name_key_str(ROOT_LEASE_NAME)
        {
            return Err(RecoveryCodecError);
        }
        let footprint = footprint_keys(self);
        if footprint.iter().collect::<BTreeSet<_>>().len() != footprint.len() {
            return Err(RecoveryCodecError);
        }
        for proof in [self.old, self.new].into_iter().flatten() {
            if proof.size > MAX_RECOVERABLE_FILE_BYTES {
                return Err(RecoveryCodecError);
            }
        }
        let fields_are_valid = match self.phase {
            RecoveryPhase::StagePrepared => self.new.is_none(),
            RecoveryPhase::StageSealed => self.new.is_some(),
            RecoveryPhase::ReplacePrepared => self.old.is_some() && self.new.is_some(),
            RecoveryPhase::PublishPrepared
            | RecoveryPhase::RemovePrepared
            | RecoveryPhase::RemoveCommitted => self.new.is_some(),
        };
        if !fields_are_valid {
            return Err(RecoveryCodecError);
        }
        Ok(())
    }
}

pub(crate) fn recovery_stage_leaf(operation_id: [u8; 16]) -> RecoveryName {
    recovery_private_leaf(RECOVERY_STAGE_PREFIX, operation_id)
}

pub(crate) fn recovery_park_leaf(operation_id: [u8; 16]) -> RecoveryName {
    recovery_private_leaf(RECOVERY_PARK_PREFIX, operation_id)
}

fn recovery_private_leaf(prefix: &str, operation_id: [u8; 16]) -> RecoveryName {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = Vec::with_capacity(prefix.len() + operation_id.len() * 2);
    bytes.extend_from_slice(prefix.as_bytes());
    for byte in operation_id {
        bytes.push(HEX[usize::from(byte >> 4)]);
        bytes.push(HEX[usize::from(byte & 0x0f)]);
    }
    let value = String::from_utf8(bytes).expect("fixed ASCII recovery leaf");
    RecoveryName::new_exact(value).expect("operation-derived recovery leaf is portable")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecoveryFrame {
    generation: u64,
    /// `None` is the canonical tombstone for a reusable slot.
    pub(crate) record: Option<RecoveryRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedRecoveryFrame {
    lane_nonce: [u8; 16],
    frame: RecoveryFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryRegistration {
    pub(crate) slot: u8,
    pub(crate) operation_id: [u8; 16],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UncertainRecoveryWrite {
    registration: RecoveryRegistration,
    predecessor: Option<RecoveryFrame>,
    intended: RecoveryFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UncertainRecoverySelection {
    Predecessor,
    Intended,
}

#[derive(Debug)]
pub(crate) struct RecoveryJournal {
    lane_nonce: [u8; 16],
    slots: [Option<RecoveryFrame>; RECOVERY_SLOT_COUNT],
    uncertain: Option<UncertainRecoveryWrite>,
}

impl RecoveryJournal {
    pub(crate) fn load(lease: &platform::LeaseHandle) -> io::Result<Self> {
        Self::load_initialized(lease, true)
    }

    fn load_initialized(
        lease: &platform::LeaseHandle,
        mut initialize_if_absent: bool,
    ) -> io::Result<Self> {
        let control_len = usize::try_from(RECOVERY_CONTROL_BYTES)
            .map_err(|_| io::Error::other("recovery control length is not representable"))?;
        let mut control = vec![0; control_len];
        loop {
            let Err(error) = platform::recovery_control_read_exact_at(lease, 0, &mut control)
            else {
                break;
            };
            if !initialize_if_absent {
                return Err(error);
            }
            platform::recovery_control_initialize_len(lease)?;
            initialize_if_absent = false;
        }
        if initialize_if_absent && control.iter().all(|byte| *byte == 0) {
            platform::recovery_control_sync(lease).map_err(|error| error.into_error())?;
            platform::recovery_control_read_exact_at(lease, 0, &mut control)?;
        }
        let mut lane_nonce = None;
        let mut slots: [Option<RecoveryFrame>; RECOVERY_SLOT_COUNT] = std::array::from_fn(|_| None);
        for (slot, selected) in slots.iter_mut().enumerate() {
            let slot = u8::try_from(slot).expect("fixed recovery slot count fits u8");
            let first = recovery_frame_region(&control, slot, 0)?;
            let second = recovery_frame_region(&control, slot, 1)?;
            if let Some(frame) = select_unbound_recovery_frame(first, second, slot)? {
                if lane_nonce.is_some_and(|nonce| nonce != frame.lane_nonce) {
                    return Err(codec_io_error());
                }
                lane_nonce = Some(frame.lane_nonce);
                *selected = Some(frame.frame);
            }
        }
        let reserved_start = usize::try_from(RECOVERY_REGION_BYTES)
            .map_err(|_| io::Error::other("recovery region length is not representable"))?;
        if control[reserved_start..].iter().any(|byte| *byte != 0) {
            return Err(codec_io_error());
        }
        validate_lane_slots(&slots, None)?;
        let lane_nonce = lane_nonce.unwrap_or_else(random_nonzero_id);
        Ok(Self {
            lane_nonce,
            slots,
            uncertain: None,
        })
    }

    pub(crate) fn records(&self) -> impl Iterator<Item = (RecoveryRegistration, &RecoveryRecord)> {
        self.slots.iter().enumerate().filter_map(|(slot, frame)| {
            let record = frame.as_ref()?.record.as_ref()?;
            Some((
                RecoveryRegistration {
                    slot: u8::try_from(slot).expect("fixed recovery slot count fits u8"),
                    operation_id: record.operation_id,
                },
                record,
            ))
        })
    }

    pub(crate) fn has_live_or_uncertain(&self) -> bool {
        self.uncertain.is_some() || self.records().next().is_some()
    }

    pub(crate) fn is_uncertain(&self) -> bool {
        self.uncertain.is_some()
    }

    pub(crate) fn reconcile_uncertain(&mut self, lease: &platform::LeaseHandle) -> io::Result<()> {
        let Some(pending) = self.uncertain.clone() else {
            return Ok(());
        };
        platform::recovery_control_sync(lease).map_err(|error| error.into_error())?;
        match self.read_uncertain_selection(lease, &pending)? {
            UncertainRecoverySelection::Intended => self.accept_uncertain_intended(pending),
            UncertainRecoverySelection::Predecessor => {
                let address = RecoveryFrameAddress::new(
                    self.lane_nonce,
                    pending.registration.slot,
                    generation_side(pending.intended.generation),
                )?;
                let encoded = encode_recovery_frame(&pending.intended, address)?;
                let offset = recovery_frame_offset(address.slot, address.frame)?;
                platform::recovery_control_write_all_at(lease, offset, &encoded)?;
                platform::recovery_control_sync(lease).map_err(|error| error.into_error())?;
                if self.read_uncertain_selection(lease, &pending)?
                    != UncertainRecoverySelection::Intended
                {
                    return Err(codec_io_error());
                }
                self.accept_uncertain_intended(pending)
            }
        }
    }

    fn read_uncertain_selection(
        &self,
        lease: &platform::LeaseHandle,
        pending: &UncertainRecoveryWrite,
    ) -> io::Result<UncertainRecoverySelection> {
        let control_len = usize::try_from(RECOVERY_CONTROL_BYTES)
            .map_err(|_| io::Error::other("recovery control length is not representable"))?;
        let mut control = vec![0; control_len];
        platform::recovery_control_read_exact_at(lease, 0, &mut control)?;
        validate_uncertain_control(&control, self, pending).map_err(Into::into)
    }

    fn accept_uncertain_intended(&mut self, pending: UncertainRecoveryWrite) -> io::Result<()> {
        let slot = usize::from(pending.registration.slot);
        if self.slots.get(slot) != Some(&pending.predecessor) {
            return Err(codec_io_error());
        }
        self.slots[slot] = Some(pending.intended);
        self.uncertain = None;
        Ok(())
    }

    pub(crate) fn record(&self, registration: RecoveryRegistration) -> Option<&RecoveryRecord> {
        self.slots
            .get(usize::from(registration.slot))?
            .as_ref()?
            .record
            .as_ref()
            .filter(|record| record.operation_id == registration.operation_id)
    }

    #[cfg(test)]
    pub(crate) fn reserve(&self, record: &RecoveryRecord) -> io::Result<RecoveryRegistration> {
        let slot = self
            .slots
            .iter()
            .position(|frame| frame.as_ref().is_none_or(|frame| frame.record.is_none()))
            .and_then(|slot| u8::try_from(slot).ok())
            .ok_or_else(codec_io_error)?;
        let registration = RecoveryRegistration {
            slot,
            operation_id: record.operation_id,
        };
        let previous = self.slots[usize::from(slot)].as_ref();
        let next = RecoveryFrame {
            generation: next_recovery_generation(previous.map(|frame| frame.generation))?,
            record: Some(record.clone()),
        };
        self.validate_candidate(registration, &next)?;
        Ok(registration)
    }

    pub(crate) fn create_reserved(
        &mut self,
        lease: &platform::LeaseHandle,
        registration: RecoveryRegistration,
        record: RecoveryRecord,
    ) -> io::Result<()> {
        self.write(lease, registration, Some(record))
    }

    pub(crate) fn advance(
        &mut self,
        lease: &platform::LeaseHandle,
        registration: RecoveryRegistration,
        record: RecoveryRecord,
    ) -> io::Result<()> {
        self.write(lease, registration, Some(record))
    }

    pub(crate) fn clear(
        &mut self,
        lease: &platform::LeaseHandle,
        registration: RecoveryRegistration,
    ) -> io::Result<()> {
        self.write(lease, registration, None)
    }

    fn write(
        &mut self,
        lease: &platform::LeaseHandle,
        registration: RecoveryRegistration,
        record: Option<RecoveryRecord>,
    ) -> io::Result<()> {
        self.write_with(
            registration,
            record,
            |offset, bytes| platform::recovery_control_write_all_at(lease, offset, bytes),
            || {
                #[cfg(test)]
                if take_pre_barrier_sync_failure() {
                    return Err((io::Error::other("injected recovery sync failure"), false));
                }
                platform::recovery_control_sync(lease)
                    .map_err(|error| error.into_error_and_barrier_state())
            },
            |offset, bytes| platform::recovery_control_read_exact_at(lease, offset, bytes),
            || Self::load_initialized(lease, false),
        )
    }

    fn write_with(
        &mut self,
        registration: RecoveryRegistration,
        record: Option<RecoveryRecord>,
        write: impl FnOnce(u64, &[u8]) -> io::Result<()>,
        sync: impl FnOnce() -> std::result::Result<(), (io::Error, bool)>,
        read: impl FnOnce(u64, &mut [u8]) -> io::Result<()>,
        reload: impl FnOnce() -> io::Result<Self>,
    ) -> io::Result<()> {
        let slot = usize::from(registration.slot);
        let previous = self.slots.get(slot).ok_or_else(codec_io_error)?.clone();
        let frame = RecoveryFrame {
            generation: next_recovery_generation(previous.as_ref().map(|frame| frame.generation))?,
            record,
        };
        self.validate_candidate(registration, &frame)?;
        let address = RecoveryFrameAddress::new(
            self.lane_nonce,
            registration.slot,
            generation_side(frame.generation),
        )?;
        let encoded = encode_recovery_frame(&frame, address)?;
        let pending = UncertainRecoveryWrite {
            registration,
            predecessor: previous,
            intended: frame.clone(),
        };
        let offset = recovery_frame_offset(address.slot, address.frame)?;
        if let Err(error) = write(offset, &encoded) {
            self.uncertain = Some(pending);
            return Err(error);
        }
        if let Err((error, barrier_confirmed)) = sync() {
            if !barrier_confirmed {
                self.uncertain = Some(pending);
                return Err(error);
            }
            return self.reconcile_confirmed_write(slot, pending, error, reload);
        }
        let mut observed = [0; RECOVERY_FRAME_BYTES];
        if let Err(error) = read(offset, &mut observed) {
            return self.reconcile_confirmed_write(slot, pending, error, reload);
        }
        match decode_recovery_frame(&observed, address) {
            Ok(observed) if observed == frame => {
                self.slots[slot] = Some(frame);
                Ok(())
            }
            Ok(_) | Err(_) => {
                self.reconcile_confirmed_write(slot, pending, codec_io_error(), reload)
            }
        }
    }

    fn reconcile_confirmed_write(
        &mut self,
        slot: usize,
        pending: UncertainRecoveryWrite,
        error: io::Error,
        reload: impl FnOnce() -> io::Result<Self>,
    ) -> io::Result<()> {
        match reload() {
            Ok(reloaded) => {
                let durable = reloaded.slots[slot].as_ref() == Some(&pending.intended)
                    && reloaded
                        .slots
                        .iter()
                        .enumerate()
                        .all(|(index, frame)| index == slot || frame == &self.slots[index]);
                if durable {
                    *self = reloaded;
                    Ok(())
                } else {
                    self.uncertain = Some(pending);
                    Err(error)
                }
            }
            Err(_) => {
                self.uncertain = Some(pending);
                Err(error)
            }
        }
    }

    fn validate_candidate(
        &self,
        registration: RecoveryRegistration,
        frame: &RecoveryFrame,
    ) -> io::Result<()> {
        let slot = usize::from(registration.slot);
        let previous = self.slots.get(slot).ok_or_else(codec_io_error)?.as_ref();
        let previous_record = previous.and_then(|frame| frame.record.as_ref());
        if self.uncertain.is_some()
            || registration.operation_id == [0; 16]
            || previous_record
                .is_some_and(|record| record.operation_id != registration.operation_id)
            || (frame.record.is_none() && previous_record.is_none())
            || frame
                .record
                .as_ref()
                .is_some_and(|record| record.operation_id != registration.operation_id)
        {
            return Err(codec_io_error());
        }
        validate_recovery_advance(previous, frame)?;
        validate_lane_slots(&self.slots, Some((slot, frame)))?;
        Ok(())
    }
}

fn recovery_frame_region(control: &[u8], slot: u8, frame: u8) -> io::Result<&[u8]> {
    let start = usize::try_from(recovery_frame_offset(slot, frame)?)
        .map_err(|_| io::Error::other("recovery frame offset is not representable"))?;
    let end = start
        .checked_add(RECOVERY_FRAME_BYTES)
        .ok_or_else(|| io::Error::other("recovery frame extent overflowed"))?;
    control.get(start..end).ok_or_else(codec_io_error)
}

fn validate_uncertain_control(
    control: &[u8],
    journal: &RecoveryJournal,
    pending: &UncertainRecoveryWrite,
) -> Result<UncertainRecoverySelection> {
    if control.len() != usize::try_from(RECOVERY_CONTROL_BYTES).map_err(|_| RecoveryCodecError)? {
        return Err(RecoveryCodecError);
    }
    let pending_slot = usize::from(pending.registration.slot);
    if journal.slots.get(pending_slot) != Some(&pending.predecessor)
        || pending
            .intended
            .record
            .as_ref()
            .is_some_and(|record| record.operation_id != pending.registration.operation_id)
        || validate_recovery_advance(pending.predecessor.as_ref(), &pending.intended).is_err()
    {
        return Err(RecoveryCodecError);
    }

    let mut candidate = journal.slots.clone();
    let mut pending_selection = None;
    for (slot, candidate_slot) in candidate.iter_mut().enumerate() {
        let slot_u8 = u8::try_from(slot).map_err(|_| RecoveryCodecError)?;
        let first = recovery_frame_region(control, slot_u8, 0).map_err(|_| RecoveryCodecError)?;
        let second = recovery_frame_region(control, slot_u8, 1).map_err(|_| RecoveryCodecError)?;
        if slot == pending_slot {
            let selection = select_uncertain_slot(first, second, journal.lane_nonce, pending)?;
            *candidate_slot = match selection {
                UncertainRecoverySelection::Predecessor => pending.predecessor.clone(),
                UncertainRecoverySelection::Intended => Some(pending.intended.clone()),
            };
            pending_selection = Some(selection);
            continue;
        }
        let selected = select_unbound_recovery_frame(first, second, slot_u8)?;
        if selected
            .as_ref()
            .is_some_and(|selected| selected.lane_nonce != journal.lane_nonce)
            || selected.as_ref().map(|selected| &selected.frame) != journal.slots[slot].as_ref()
        {
            return Err(RecoveryCodecError);
        }
    }
    let reserved_start = usize::try_from(RECOVERY_REGION_BYTES).map_err(|_| RecoveryCodecError)?;
    if control[reserved_start..].iter().any(|byte| *byte != 0) {
        return Err(RecoveryCodecError);
    }
    validate_lane_slots(&candidate, None)?;
    pending_selection.ok_or(RecoveryCodecError)
}

fn select_uncertain_slot(
    first: &[u8],
    second: &[u8],
    lane_nonce: [u8; 16],
    pending: &UncertainRecoveryWrite,
) -> Result<UncertainRecoverySelection> {
    match select_unbound_recovery_frame(first, second, pending.registration.slot) {
        Ok(Some(selected))
            if selected.lane_nonce == lane_nonce
                && Some(&selected.frame) == pending.predecessor.as_ref() =>
        {
            Ok(UncertainRecoverySelection::Predecessor)
        }
        Ok(Some(selected))
            if selected.lane_nonce == lane_nonce && selected.frame == pending.intended =>
        {
            Ok(UncertainRecoverySelection::Intended)
        }
        Ok(None) if pending.predecessor.is_none() => Ok(UncertainRecoverySelection::Predecessor),
        Err(_) if pending.predecessor.is_none() => {
            let intended_side = generation_side(pending.intended.generation);
            let (intended_bytes, other_bytes) = if intended_side == 0 {
                (first, second)
            } else {
                (second, first)
            };
            if other_bytes.iter().any(|byte| *byte != 0) {
                return Err(RecoveryCodecError);
            }
            let address =
                RecoveryFrameAddress::new(lane_nonce, pending.registration.slot, intended_side)?;
            match decode_recovery_frame(intended_bytes, address) {
                Ok(frame) if frame == pending.intended => Ok(UncertainRecoverySelection::Intended),
                Err(_) => Ok(UncertainRecoverySelection::Predecessor),
                Ok(_) => Err(RecoveryCodecError),
            }
        }
        _ => Err(RecoveryCodecError),
    }
}

impl RecoveryFrame {
    fn validate(&self) -> Result<()> {
        if self.generation == 0 {
            return Err(RecoveryCodecError);
        }
        if let Some(record) = &self.record {
            record.validate()?;
        }
        Ok(())
    }
}

fn next_recovery_generation(current: Option<u64>) -> Result<u64> {
    match current {
        None => Ok(1),
        Some(0) => Err(RecoveryCodecError),
        Some(generation) => generation.checked_add(1).ok_or(RecoveryCodecError),
    }
}

fn validate_recovery_advance(previous: Option<&RecoveryFrame>, next: &RecoveryFrame) -> Result<()> {
    next.validate()?;
    if next.generation != next_recovery_generation(previous.map(|frame| frame.generation))? {
        return Err(RecoveryCodecError);
    }
    let valid = match (
        previous.and_then(|frame| frame.record.as_ref()),
        &next.record,
    ) {
        (None, Some(next)) => next.phase == RecoveryPhase::StagePrepared,
        (Some(previous), None) => previous.phase != RecoveryPhase::StageSealed,
        (Some(previous), Some(next)) => {
            previous.operation_id == next.operation_id
                && previous.destination_parent == next.destination_parent
                && previous.destination_leaf == next.destination_leaf
                && previous.old == next.old
                && previous.new.is_none_or(|proof| next.new == Some(proof))
                && matches!(
                    (previous.phase, next.phase, previous.old),
                    (RecoveryPhase::StagePrepared, RecoveryPhase::StageSealed, _)
                        | (
                            RecoveryPhase::StageSealed,
                            RecoveryPhase::ReplacePrepared,
                            Some(_)
                        )
                        | (
                            RecoveryPhase::StageSealed,
                            RecoveryPhase::PublishPrepared,
                            None
                        )
                        | (
                            RecoveryPhase::ReplacePrepared,
                            RecoveryPhase::PublishPrepared,
                            Some(_)
                        )
                        | (
                            RecoveryPhase::ReplacePrepared,
                            RecoveryPhase::RemovePrepared,
                            Some(_)
                        )
                        | (
                            RecoveryPhase::PublishPrepared,
                            RecoveryPhase::RemoveCommitted,
                            _
                        )
                        | (RecoveryPhase::StageSealed, RecoveryPhase::RemovePrepared, _)
                        | (
                            RecoveryPhase::PublishPrepared,
                            RecoveryPhase::RemovePrepared,
                            _
                        )
                )
        }
        (None, None) => false,
    };
    valid.then_some(()).ok_or(RecoveryCodecError)
}

fn generation_side(generation: u64) -> u8 {
    u8::from(generation.is_multiple_of(2))
}

fn validate_lane_slots(
    slots: &[Option<RecoveryFrame>],
    change: Option<(usize, &RecoveryFrame)>,
) -> Result<()> {
    if slots.len() != RECOVERY_SLOT_COUNT {
        return Err(RecoveryCodecError);
    }
    let mut operation_ids = BTreeSet::new();
    let mut footprints = BTreeSet::new();
    let mut live_proof_bytes = 0_u64;
    for (slot, selected) in slots.iter().enumerate() {
        let frame = if change.is_some_and(|(changed, _)| changed == slot) {
            change.map(|(_, frame)| frame)
        } else {
            selected.as_ref()
        };
        let Some(frame) = frame else { continue };
        if frame.generation == u64::MAX {
            return Err(RecoveryCodecError);
        }
        let Some(record) = &frame.record else {
            continue;
        };
        if !operation_ids.insert(record.operation_id)
            || footprint_keys(record)
                .into_iter()
                .any(|coordinate| !footprints.insert(coordinate))
        {
            return Err(RecoveryCodecError);
        }
        for proof in [record.old, record.new].into_iter().flatten() {
            live_proof_bytes += proof.size;
        }
        if live_proof_bytes > MAX_LIVE_PROOF_BYTES {
            return Err(RecoveryCodecError);
        }
    }
    Ok(())
}

fn footprint_keys(record: &RecoveryRecord) -> Vec<String> {
    let mut footprint = vec![coordinate_key(
        &record.destination_parent,
        &recovery_stage_leaf(record.operation_id),
    )];
    if record.old.is_some() && record.phase.owns_park() {
        footprint.push(coordinate_key(
            &record.destination_parent,
            &recovery_park_leaf(record.operation_id),
        ));
    }
    if record.phase.owns_target() {
        footprint.push(coordinate_key(
            &record.destination_parent,
            &record.destination_leaf,
        ));
    }
    footprint
}

fn recovery_frame_offset(slot: u8, frame: u8) -> Result<u64> {
    if usize::from(slot) >= RECOVERY_SLOT_COUNT || usize::from(frame) >= RECOVERY_FRAMES_PER_SLOT {
        return Err(RecoveryCodecError);
    }
    let ordinal = usize::from(slot) * RECOVERY_FRAMES_PER_SLOT + usize::from(frame);
    let offset = ordinal
        .checked_mul(RECOVERY_FRAME_BYTES)
        .ok_or(RecoveryCodecError)?;
    u64::try_from(offset).map_err(|_| RecoveryCodecError)
}

fn encode_recovery_frame(
    frame: &RecoveryFrame,
    address: RecoveryFrameAddress,
) -> Result<[u8; RECOVERY_FRAME_BYTES]> {
    frame.validate()?;
    if address.frame != generation_side(frame.generation) {
        return Err(RecoveryCodecError);
    }

    let mut payload = Vec::new();
    let kind = if let Some(record) = &frame.record {
        encode_record(record, &mut payload);
        1
    } else {
        0
    };
    let payload_len = u32::try_from(payload.len()).map_err(|_| RecoveryCodecError)?;
    if HEADER_BYTES
        .checked_add(payload.len())
        .ok_or(RecoveryCodecError)?
        > FRAME_BODY_BYTES
    {
        return Err(RecoveryCodecError);
    }

    let mut encoded = [0; RECOVERY_FRAME_BYTES];
    let mut header = Vec::with_capacity(HEADER_BYTES);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&SCHEMA.to_le_bytes());
    header.push(PLATFORM_TAG);
    header.push(kind);
    header.extend_from_slice(&address.lane_nonce);
    header.push(address.slot);
    header.push(address.frame);
    header.extend_from_slice(&[0; 6]);
    header.extend_from_slice(&frame.generation.to_le_bytes());
    header.extend_from_slice(&payload_len.to_le_bytes());
    header.extend_from_slice(&[0; 4]);
    debug_assert_eq!(header.len(), HEADER_BYTES);
    encoded[..HEADER_BYTES].copy_from_slice(&header);
    encoded[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(&payload);
    let checksum = frame_checksum(&encoded[..FRAME_BODY_BYTES], address.frame);
    encoded[FRAME_BODY_BYTES..].copy_from_slice(&checksum);
    Ok(encoded)
}

fn decode_recovery_frame(encoded: &[u8], address: RecoveryFrameAddress) -> Result<RecoveryFrame> {
    if encoded.len() != RECOVERY_FRAME_BYTES {
        return Err(RecoveryCodecError);
    }
    let (body, checksum) = encoded.split_at(FRAME_BODY_BYTES);
    if checksum != frame_checksum(body, address.frame) {
        return Err(RecoveryCodecError);
    }

    let mut cursor = Cursor::new(body);
    if cursor.take(MAGIC.len())? != MAGIC || cursor.u16()? != SCHEMA || cursor.u8()? != PLATFORM_TAG
    {
        return Err(RecoveryCodecError);
    }
    let kind = cursor.u8()?;
    if cursor.array::<16>()? != address.lane_nonce
        || cursor.u8()? != address.slot
        || cursor.u8()? != address.frame
        || cursor.take(6)?.iter().any(|byte| *byte != 0)
    {
        return Err(RecoveryCodecError);
    }
    let generation = cursor.u64()?;
    if generation == 0 || address.frame != generation_side(generation) {
        return Err(RecoveryCodecError);
    }
    let payload_len = usize::try_from(cursor.u32()?).map_err(|_| RecoveryCodecError)?;
    if cursor.take(4)?.iter().any(|byte| *byte != 0) {
        return Err(RecoveryCodecError);
    }
    let payload = cursor.take(payload_len)?;
    if cursor.remaining.iter().any(|byte| *byte != 0) {
        return Err(RecoveryCodecError);
    }
    let record = match kind {
        0 if payload.is_empty() => None,
        1 => Some(decode_record(payload)?),
        _ => return Err(RecoveryCodecError),
    };
    let frame = RecoveryFrame { generation, record };
    frame.validate()?;
    Ok(frame)
}

#[cfg(test)]
fn select_recovery_frame(
    first: &[u8],
    second: &[u8],
    lane_nonce: [u8; 16],
    slot: u8,
) -> Result<Option<SelectedRecoveryFrame>> {
    let selected = select_unbound_recovery_frame(first, second, slot)?;
    if selected
        .as_ref()
        .is_some_and(|selected| selected.lane_nonce != lane_nonce)
    {
        return Err(RecoveryCodecError);
    }
    Ok(selected)
}

fn probe_recovery_frame(
    encoded: &[u8],
    slot: u8,
    side: u8,
) -> Result<Option<SelectedRecoveryFrame>> {
    if encoded.len() != RECOVERY_FRAME_BYTES {
        return Err(RecoveryCodecError);
    }
    if encoded.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let lane_nonce = encoded[12..28].try_into().map_err(|_| RecoveryCodecError)?;
    let address = RecoveryFrameAddress::new(lane_nonce, slot, side)?;
    let frame = decode_recovery_frame(encoded, address)?;
    Ok(Some(SelectedRecoveryFrame { lane_nonce, frame }))
}

fn select_unbound_recovery_frame(
    first: &[u8],
    second: &[u8],
    slot: u8,
) -> Result<Option<SelectedRecoveryFrame>> {
    let first = probe_recovery_frame(first, slot, 0);
    let second = probe_recovery_frame(second, slot, 1);
    match (first, second) {
        (Ok(None), Ok(None)) => Ok(None),
        (Ok(Some(selected)), Err(_)) | (Err(_), Ok(Some(selected))) => Ok(Some(selected)),
        (Ok(Some(selected)), Ok(None)) | (Ok(None), Ok(Some(selected))) => {
            if selected.frame.generation != 1
                || !matches!(
                    selected.frame.record.as_ref(),
                    Some(RecoveryRecord {
                        phase: RecoveryPhase::StagePrepared,
                        ..
                    })
                )
            {
                return Err(RecoveryCodecError);
            }
            Ok(Some(selected))
        }
        (Ok(Some(first)), Ok(Some(second))) => {
            if first.lane_nonce != second.lane_nonce {
                return Err(RecoveryCodecError);
            }
            match first.frame.generation.cmp(&second.frame.generation) {
                std::cmp::Ordering::Less => {
                    validate_recovery_advance(Some(&first.frame), &second.frame)?;
                    Ok(Some(second))
                }
                std::cmp::Ordering::Greater => {
                    validate_recovery_advance(Some(&second.frame), &first.frame)?;
                    Ok(Some(first))
                }
                std::cmp::Ordering::Equal => Err(RecoveryCodecError),
            }
        }
        (Err(_), Ok(None) | Err(_)) | (Ok(None), Err(_)) => Err(RecoveryCodecError),
    }
}

fn encode_record(record: &RecoveryRecord, output: &mut Vec<u8>) {
    output.extend_from_slice(&record.operation_id);
    output.push(record.phase as u8);
    output.push(u8::from(record.old.is_some()) | (u8::from(record.new.is_some()) << 1));
    output.push(
        u8::try_from(record.destination_parent.len())
            .expect("validated recovery parent count fits u8"),
    );
    output.extend_from_slice(&[0; 5]);
    for name in record
        .destination_parent
        .iter()
        .chain([&record.destination_leaf])
    {
        encode_name(name, output);
    }
    for proof in [record.old, record.new].into_iter().flatten() {
        output.extend_from_slice(&proof.size.to_le_bytes());
        output.extend_from_slice(&proof.sha256);
    }
}

fn decode_record(payload: &[u8]) -> Result<RecoveryRecord> {
    let mut cursor = Cursor::new(payload);
    let operation_id = cursor.array::<16>()?;
    let phase = RecoveryPhase::decode(cursor.u8()?)?;
    let flags = cursor.u8()?;
    if flags & !0b11 != 0 {
        return Err(RecoveryCodecError);
    }
    let destination_count = usize::from(cursor.u8()?);
    if destination_count.checked_add(1).ok_or(RecoveryCodecError)? > MAX_COMPONENTS
        || cursor.take(5)?.iter().any(|byte| *byte != 0)
    {
        return Err(RecoveryCodecError);
    }
    let destination_parent = (0..destination_count)
        .map(|_| decode_name(&mut cursor))
        .collect::<Result<_>>()?;
    let destination_leaf = decode_name(&mut cursor)?;
    let old = if flags & 1 != 0 {
        Some(decode_proof(&mut cursor)?)
    } else {
        None
    };
    let new = if flags & 2 != 0 {
        Some(decode_proof(&mut cursor)?)
    } else {
        None
    };
    if !cursor.remaining.is_empty() {
        return Err(RecoveryCodecError);
    }
    Ok(RecoveryRecord {
        operation_id,
        phase,
        destination_parent,
        destination_leaf,
        old,
        new,
    })
}

fn encode_name(name: &RecoveryName, output: &mut Vec<u8>) {
    let bytes = name.as_str().as_bytes();
    output.extend_from_slice(
        &u16::try_from(bytes.len())
            .expect("validated recovery name length fits u16")
            .to_le_bytes(),
    );
    output.extend_from_slice(bytes);
}

fn decode_name(cursor: &mut Cursor<'_>) -> Result<RecoveryName> {
    let len = usize::from(cursor.u16()?);
    let value = std::str::from_utf8(cursor.take(len)?).map_err(|_| RecoveryCodecError)?;
    RecoveryName::new_exact(value.to_string())
}

fn decode_proof(cursor: &mut Cursor<'_>) -> Result<RecoveryFileProof> {
    Ok(RecoveryFileProof {
        size: cursor.u64()?,
        sha256: cursor.array::<32>()?,
    })
}

fn validate_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > MAX_NAME_BYTES
        || value.encode_utf16().count() > MAX_NAME_UTF16_UNITS
        || value.nfc().collect::<String>() != value
        || value.bytes().any(|byte| b"<>:\"/\\|?*".contains(&byte))
        || value.chars().any(char::is_control)
        || value.ends_with(['.', ' '])
        || windows_device_name(value)
    {
        return Err(RecoveryCodecError);
    }
    Ok(())
}

fn windows_device_name(value: &str) -> bool {
    let basename = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches(['.', ' ']);
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$"]
        .iter()
        .any(|device| basename.eq_ignore_ascii_case(device))
    {
        return true;
    }
    let Some((index, digit)) = basename.char_indices().next_back() else {
        return false;
    };
    matches!(digit, '0'..='9' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}')
        && (basename[..index].eq_ignore_ascii_case("COM")
            || basename[..index].eq_ignore_ascii_case("LPT"))
}

fn coordinate_key(parent: &[RecoveryName], leaf: &RecoveryName) -> String {
    let spelling = parent
        .iter()
        .chain([leaf])
        .map(RecoveryName::as_str)
        .collect::<Vec<_>>()
        .join("/");
    spelling.case_fold().nfc().collect()
}

fn portable_name_key(name: &RecoveryName) -> String {
    portable_name_key_str(name.as_str())
}

fn portable_name_key_str(name: &str) -> String {
    name.case_fold().nfc().collect()
}

fn frame_checksum(body: &[u8], frame: u8) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHECKSUM_DOMAIN);
    hasher.update([frame]);
    hasher.update(body);
    hasher.finalize().into()
}

struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(len)
            .ok_or(RecoveryCodecError)?;
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| RecoveryCodecError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const LANE: [u8; 16] = [0x21; 16];

    fn name(value: &str) -> RecoveryName {
        RecoveryName::new_exact(value).expect("valid recovery name")
    }

    fn proof(size: u64, byte: u8) -> RecoveryFileProof {
        RecoveryFileProof {
            size,
            sha256: [byte; 32],
        }
    }

    fn record(phase: RecoveryPhase) -> RecoveryRecord {
        let (old, new) = match phase {
            RecoveryPhase::StagePrepared => (None, None),
            RecoveryPhase::StageSealed => (None, Some(proof(7, 0x31))),
            RecoveryPhase::ReplacePrepared => (Some(proof(5, 0x20)), Some(proof(7, 0x31))),
            RecoveryPhase::PublishPrepared
            | RecoveryPhase::RemovePrepared
            | RecoveryPhase::RemoveCommitted => (None, Some(proof(7, 0x31))),
        };
        let operation_id = [0x11; 16];
        let parent = vec![name("destination")];
        RecoveryRecord {
            operation_id,
            phase,
            destination_parent: parent,
            destination_leaf: name("target.bin"),
            old,
            new,
        }
    }

    fn replacement_record(phase: RecoveryPhase, proofs_equal: bool) -> RecoveryRecord {
        let old = proof(5, 0x20);
        let mut value = record(phase);
        value.old = Some(old);
        value.new = (phase != RecoveryPhase::StagePrepared).then_some(if proofs_equal {
            old
        } else {
            proof(7, 0x31)
        });
        value
    }

    type ReplacementCase = (
        bool,
        RecoveryPhase,
        ReplacementCarrier,
        ReplacementCarrier,
        ReplacementCarrier,
        ReplacementAction,
    );

    fn canonical_replacement_cases() -> Vec<ReplacementCase> {
        use RecoveryPhase::*;
        use ReplacementAction::*;
        use ReplacementCarrier::{
            Absent as A, New as N, Old as O, OldAndNew as B, Other as X, Unobserved as U,
            Unsealed as S,
        };

        let mut cases = Vec::new();
        macro_rules! add {
            ($equal:expr, $phase:expr, $stage:expr, $target:expr, $park:expr, $action:expr) => {
                cases.push(($equal, $phase, $stage, $target, $park, $action));
            };
        }
        add!(false, StagePrepared, S, U, A, RemoveStage);
        add!(false, StagePrepared, A, U, A, NoEffect);
        for proofs_equal in [false, true] {
            let (old, new) = if proofs_equal { (B, B) } else { (O, N) };

            add!(
                proofs_equal,
                StageSealed,
                new,
                old,
                A,
                Advance(ReplacePrepared)
            );
            for target in [A, X].into_iter().chain((!proofs_equal).then_some(N)) {
                add!(
                    proofs_equal,
                    StageSealed,
                    new,
                    target,
                    A,
                    Advance(RemovePrepared)
                );
            }

            add!(proofs_equal, ReplacePrepared, new, old, A, ParkTarget);
            add!(
                proofs_equal,
                ReplacePrepared,
                new,
                A,
                old,
                Advance(PublishPrepared)
            );
            for target in [A, X].into_iter().chain((!proofs_equal).then_some(N)) {
                add!(
                    proofs_equal,
                    ReplacePrepared,
                    new,
                    target,
                    A,
                    Advance(RemovePrepared)
                );
            }
            add!(proofs_equal, ReplacePrepared, A, old, A, NoEffect);
            add!(proofs_equal, ReplacePrepared, A, A, old, RestorePark);

            add!(proofs_equal, PublishPrepared, new, A, old, PublishStage);
            add!(proofs_equal, PublishPrepared, A, new, old, RemovePark);
            add!(
                proofs_equal,
                PublishPrepared,
                A,
                new,
                A,
                Advance(RemoveCommitted)
            );
            add!(
                proofs_equal,
                PublishPrepared,
                new,
                old,
                A,
                Advance(RemovePrepared)
            );
            if !proofs_equal {
                add!(false, PublishPrepared, A, old, A, NoEffect);
            }
            add!(proofs_equal, PublishPrepared, A, A, old, RestorePark);

            add!(proofs_equal, RemovePrepared, new, U, U, RemoveStage);
            add!(proofs_equal, RemovePrepared, A, U, U, NoEffect);
            add!(proofs_equal, RemoveCommitted, new, old, A, ParkTarget);
            add!(proofs_equal, RemoveCommitted, new, A, old, PublishStage);
            add!(proofs_equal, RemoveCommitted, new, A, A, PublishStage);
            add!(proofs_equal, RemoveCommitted, A, new, old, RemovePark);
            add!(proofs_equal, RemoveCommitted, A, new, A, Applied);
        }
        cases
    }

    fn address(frame: u8) -> RecoveryFrameAddress {
        RecoveryFrameAddress::new(LANE, 7, frame).expect("valid frame address")
    }

    fn empty_journal() -> RecoveryJournal {
        RecoveryJournal {
            lane_nonce: LANE,
            slots: std::array::from_fn(|_| None),
            uncertain: None,
        }
    }

    fn journal_with(slot: usize, frame: RecoveryFrame) -> RecoveryJournal {
        let mut journal = empty_journal();
        journal.slots[slot] = Some(frame);
        journal
    }

    fn encoded(generation: u64, phase: RecoveryPhase, side: u8) -> [u8; RECOVERY_FRAME_BYTES] {
        encode_recovery_frame(
            &RecoveryFrame {
                generation,
                record: Some(record(phase)),
            },
            address(side),
        )
        .expect("encodable recovery frame")
    }

    fn resign(bytes: &mut [u8; RECOVERY_FRAME_BYTES], side: u8) {
        let checksum = frame_checksum(&bytes[..FRAME_BODY_BYTES], side);
        bytes[FRAME_BODY_BYTES..].copy_from_slice(&checksum);
    }

    #[test]
    fn minimum_maximum_and_tombstone_round_trip_canonically() {
        let minimum = RecoveryFrame {
            generation: 1,
            record: Some(RecoveryRecord {
                operation_id: [1; 16],
                phase: RecoveryPhase::StagePrepared,
                destination_parent: Vec::new(),
                destination_leaf: name("d"),
                old: None,
                new: None,
            }),
        };
        let bytes = encode_recovery_frame(&minimum, address(0)).unwrap();
        assert_eq!(decode_recovery_frame(&bytes, address(0)), Ok(minimum));

        let maximum_name = |index: usize| name(&format!("{index:02}{}", "x".repeat(253)));
        let maximum_parent = (0..31).map(maximum_name).collect::<Vec<_>>();
        let maximum = RecoveryFrame {
            generation: u64::MAX - 1,
            record: Some(RecoveryRecord {
                operation_id: [0xff; 16],
                phase: RecoveryPhase::PublishPrepared,
                destination_parent: maximum_parent,
                destination_leaf: name(&"d".repeat(255)),
                old: Some(proof(MAX_RECOVERABLE_FILE_BYTES, 0xaa)),
                new: Some(proof(MAX_RECOVERABLE_FILE_BYTES, 0xbb)),
            }),
        };
        let bytes = encode_recovery_frame(&maximum, address(1)).unwrap();
        assert_eq!(decode_recovery_frame(&bytes, address(1)), Ok(maximum));

        let tombstone = RecoveryFrame {
            generation: 9,
            record: None,
        };
        let bytes = encode_recovery_frame(&tombstone, address(0)).unwrap();
        assert_eq!(decode_recovery_frame(&bytes, address(0)), Ok(tombstone));
        assert!(
            bytes[HEADER_BYTES..FRAME_BODY_BYTES]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    #[test]
    fn decoder_rejects_every_truncation_and_trailing_byte() {
        let bytes = encoded(1, RecoveryPhase::PublishPrepared, 0);
        for len in 0..RECOVERY_FRAME_BYTES {
            assert_eq!(
                decode_recovery_frame(&bytes[..len], address(0)),
                Err(RecoveryCodecError),
                "accepted truncation at {len}"
            );
        }
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert_eq!(
            decode_recovery_frame(&trailing, address(0)),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn checksum_rejects_header_payload_padding_and_checksum_drift() {
        let bytes = encoded(1, RecoveryPhase::PublishPrepared, 0);
        for offset in [0, 17, HEADER_BYTES + 5, 1_024, FRAME_BODY_BYTES] {
            let mut drifted = bytes;
            drifted[offset] ^= 1;
            assert_eq!(
                decode_recovery_frame(&drifted, address(0)),
                Err(RecoveryCodecError),
                "accepted drift at {offset}"
            );
        }
        let other_side = encoded(2, RecoveryPhase::PublishPrepared, 1);
        assert_ne!(&bytes[FRAME_BODY_BYTES..], &other_side[FRAME_BODY_BYTES..]);
        assert_ne!(
            frame_checksum(&bytes[..FRAME_BODY_BYTES], 0),
            frame_checksum(&bytes[..FRAME_BODY_BYTES], 1)
        );
        assert_eq!(
            decode_recovery_frame(&bytes, address(1)),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn decoder_rejects_schema_platform_lane_slot_frame_and_reserved_mismatch() {
        for (offset, value) in [
            (8, 2),
            (10, PLATFORM_TAG.wrapping_add(1)),
            (12, 0x22),
            (28, 8),
            (29, 1),
            (30, 1),
            (48, 1),
        ] {
            let mut drifted = encoded(1, RecoveryPhase::PublishPrepared, 0);
            drifted[offset] = value;
            resign(&mut drifted, 0);
            assert_eq!(
                decode_recovery_frame(&drifted, address(0)),
                Err(RecoveryCodecError),
                "accepted field drift at {offset}"
            );
        }
        assert!(RecoveryFrameAddress::new([0; 16], 0, 0).is_err());
        assert!(RecoveryFrameAddress::new(LANE, 64, 0).is_err());
        assert!(RecoveryFrameAddress::new(LANE, 0, 2).is_err());
    }

    #[test]
    fn decoder_rejects_unknown_kind_phase_flags_lengths_and_payload_reserved_data() {
        let mut cases = Vec::new();
        let mut unknown_kind = encoded(1, RecoveryPhase::PublishPrepared, 0);
        unknown_kind[11] = 7;
        cases.push(unknown_kind);
        let mut unknown_phase = encoded(1, RecoveryPhase::PublishPrepared, 0);
        unknown_phase[HEADER_BYTES + 16] = 7;
        cases.push(unknown_phase);
        let mut unknown_flags = encoded(1, RecoveryPhase::PublishPrepared, 0);
        unknown_flags[HEADER_BYTES + 17] = 0x80;
        cases.push(unknown_flags);
        let mut too_many_components = encoded(1, RecoveryPhase::PublishPrepared, 0);
        too_many_components[HEADER_BYTES + 18] = 33;
        cases.push(too_many_components);
        let mut payload_reserved = encoded(1, RecoveryPhase::PublishPrepared, 0);
        payload_reserved[HEADER_BYTES + 20] = 1;
        cases.push(payload_reserved);
        let mut excessive_payload = encoded(1, RecoveryPhase::PublishPrepared, 0);
        excessive_payload[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(excessive_payload);
        let mut excessive_name = encoded(1, RecoveryPhase::PublishPrepared, 0);
        excessive_name[HEADER_BYTES + 24..HEADER_BYTES + 26]
            .copy_from_slice(&u16::MAX.to_le_bytes());
        cases.push(excessive_name);
        let mut noncanonical_padding = encoded(1, RecoveryPhase::PublishPrepared, 0);
        noncanonical_padding[1_024] = 1;
        cases.push(noncanonical_padding);
        let mut zero_generation = encoded(1, RecoveryPhase::PublishPrepared, 0);
        zero_generation[36..44].fill(0);
        cases.push(zero_generation);
        for mut case in cases {
            resign(&mut case, 0);
            assert_eq!(
                decode_recovery_frame(&case, address(0)),
                Err(RecoveryCodecError)
            );
        }
    }

    #[test]
    fn decoder_rejects_resigned_noncanonical_lengths_and_oversized_file_proofs() {
        let original = encoded(1, RecoveryPhase::PublishPrepared, 0);
        let payload_len = u32::from_le_bytes(original[44..48].try_into().unwrap());
        for changed_len in [payload_len - 1, payload_len + 1] {
            let mut changed = original;
            changed[44..48].copy_from_slice(&changed_len.to_le_bytes());
            resign(&mut changed, 0);
            assert_eq!(
                decode_recovery_frame(&changed, address(0)),
                Err(RecoveryCodecError)
            );
        }

        let mut oversized = original;
        let proof_size = HEADER_BYTES + 24 + (2 + "destination".len()) + (2 + "target.bin".len());
        oversized[proof_size..proof_size + 8]
            .copy_from_slice(&(MAX_RECOVERABLE_FILE_BYTES + 1).to_le_bytes());
        resign(&mut oversized, 0);
        assert_eq!(
            decode_recovery_frame(&oversized, address(0)),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn selection_uses_highest_valid_generation_and_preserves_a_valid_predecessor() {
        let first = encoded(1, RecoveryPhase::StagePrepared, 0);
        let second = encoded(2, RecoveryPhase::StageSealed, 1);
        let selected = select_recovery_frame(&first, &second, LANE, 7)
            .unwrap()
            .unwrap();
        assert_eq!(selected.frame.generation, 2);

        let mut torn = second;
        torn[HEADER_BYTES] ^= 1;
        let selected = select_recovery_frame(&first, &torn, LANE, 7)
            .unwrap()
            .unwrap();
        assert_eq!(selected.frame.generation, 1);
        assert_eq!(
            select_recovery_frame(&[0; 8], &[0; 8], LANE, 7),
            Err(RecoveryCodecError)
        );
        assert_eq!(
            select_recovery_frame(
                &[0; RECOVERY_FRAME_BYTES],
                &[0; RECOVERY_FRAME_BYTES],
                LANE,
                7,
            ),
            Ok(None)
        );

        let zero = [0; RECOVERY_FRAME_BYTES];
        assert_eq!(
            select_recovery_frame(&torn, &zero, LANE, 7),
            Err(RecoveryCodecError)
        );
        assert_eq!(
            select_recovery_frame(&first, &zero, [0x22; 16], 7),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn selection_rejects_impossible_dual_frame_histories() {
        let first = encoded(1, RecoveryPhase::StagePrepared, 0);
        let generation_gap = encoded(4, RecoveryPhase::StageSealed, 1);
        assert_eq!(
            select_recovery_frame(&first, &generation_gap, LANE, 7),
            Err(RecoveryCodecError)
        );
        let skipped_phase = encoded(2, RecoveryPhase::PublishPrepared, 1);
        assert_eq!(
            select_recovery_frame(&first, &skipped_phase, LANE, 7),
            Err(RecoveryCodecError)
        );
        let other_lane = encode_recovery_frame(
            &RecoveryFrame {
                generation: 2,
                record: Some(record(RecoveryPhase::StageSealed)),
            },
            RecoveryFrameAddress::new([0x22; 16], 7, 1).unwrap(),
        )
        .unwrap();
        assert_eq!(
            select_recovery_frame(&first, &other_lane, LANE, 7),
            Err(RecoveryCodecError)
        );

        let tombstone = RecoveryFrame {
            generation: 2,
            record: None,
        };
        let tombstone = encode_recovery_frame(&tombstone, address(1)).unwrap();
        assert_eq!(
            select_recovery_frame(&first, &tombstone, LANE, 7)
                .unwrap()
                .unwrap()
                .frame
                .record,
            None
        );
    }

    #[test]
    fn generations_never_wrap_and_offsets_cover_exactly_the_fixed_lane() {
        assert_eq!(next_recovery_generation(None), Ok(1));
        assert_eq!(next_recovery_generation(Some(0)), Err(RecoveryCodecError));
        assert_eq!(next_recovery_generation(Some(41)), Ok(42));
        assert_eq!(
            next_recovery_generation(Some(u64::MAX)),
            Err(RecoveryCodecError)
        );
        assert_eq!(recovery_frame_offset(0, 0), Ok(0));
        assert_eq!(
            recovery_frame_offset(63, 1),
            Ok(RECOVERY_REGION_BYTES - RECOVERY_FRAME_BYTES as u64)
        );
        assert_eq!(RECOVERY_REGION_BYTES, 2 * 1024 * 1024);
        assert_eq!(RECOVERY_CONTROL_BYTES, 4 * 1024 * 1024);
        assert!(recovery_frame_offset(64, 0).is_err());
        assert!(recovery_frame_offset(0, 2).is_err());

        let predecessor = encoded(u64::MAX - 1, RecoveryPhase::PublishPrepared, 1);
        let maximum = encoded(u64::MAX, RecoveryPhase::RemoveCommitted, 0);
        let selected = select_recovery_frame(&maximum, &predecessor, LANE, 7)
            .unwrap()
            .unwrap();
        assert_eq!(selected.frame.generation, u64::MAX);
        assert_eq!(
            next_recovery_generation(Some(selected.frame.generation)),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn lane_validation_rejects_duplicate_owners_overlaps_budget_and_exhaustion() {
        let mut slots: [Option<RecoveryFrame>; RECOVERY_SLOT_COUNT] = std::array::from_fn(|_| None);
        let mut first = record(RecoveryPhase::StageSealed);
        first.new = Some(proof(MAX_RECOVERABLE_FILE_BYTES, 1));
        slots[0] = Some(RecoveryFrame {
            generation: 1,
            record: Some(first.clone()),
        });
        assert!(validate_lane_slots(&slots, None).is_ok());

        let mut duplicate = first.clone();
        duplicate.destination_parent = vec![name("elsewhere")];
        duplicate.destination_leaf = name("other.bin");
        slots[1] = Some(RecoveryFrame {
            generation: 1,
            record: Some(duplicate),
        });
        assert!(validate_lane_slots(&slots, None).is_err());

        let mut overlap = first.clone();
        overlap.operation_id = [2; 16];
        overlap.destination_leaf = name("TARGET.BIN");
        slots[1] = Some(RecoveryFrame {
            generation: 1,
            record: Some(overlap),
        });
        assert!(validate_lane_slots(&slots, None).is_err());

        for (slot, entry) in slots.iter_mut().enumerate().take(9) {
            let mut value = first.clone();
            value.operation_id = [u8::try_from(slot + 1).unwrap(); 16];
            value.destination_parent = vec![name(&format!("parent-{slot}"))];
            value.destination_leaf = name(&format!("target-{slot}.bin"));
            *entry = Some(RecoveryFrame {
                generation: 1,
                record: Some(value),
            });
        }
        assert!(validate_lane_slots(&slots, None).is_err());
        slots[8] = None;
        assert!(validate_lane_slots(&slots, None).is_ok());
        slots[0].as_mut().unwrap().generation = u64::MAX;
        assert!(validate_lane_slots(&slots, None).is_err());
    }

    #[test]
    fn footprints_follow_phase_and_replacement_ownership() {
        let prepared = record(RecoveryPhase::StagePrepared);
        let target = coordinate_key(&prepared.destination_parent, &prepared.destination_leaf);
        let park = coordinate_key(
            &prepared.destination_parent,
            &recovery_park_leaf(prepared.operation_id),
        );
        let prepared_footprint = footprint_keys(&prepared);
        assert!(!prepared_footprint.contains(&target));
        assert!(!prepared_footprint.contains(&park));

        let sealed = record(RecoveryPhase::StageSealed);
        assert!(footprint_keys(&sealed).contains(&target));
        assert!(!footprint_keys(&sealed).contains(&park));

        let removed = record(RecoveryPhase::RemovePrepared);
        assert!(!footprint_keys(&removed).contains(&target));
        assert!(!footprint_keys(&removed).contains(&park));

        let mut replacement = prepared;
        replacement.old = Some(proof(3, 0x40));
        assert!(footprint_keys(&replacement).contains(&park));
        assert!(!footprint_keys(&replacement).contains(&target));

        replacement.phase = RecoveryPhase::RemovePrepared;
        replacement.new = Some(proof(4, 0x41));
        assert!(!footprint_keys(&replacement).contains(&park));
        assert!(!footprint_keys(&replacement).contains(&target));
    }

    #[test]
    fn replacement_phase_projection_owns_exact_coordinates() {
        for (phase, target, park) in [
            (RecoveryPhase::StagePrepared, false, true),
            (RecoveryPhase::StageSealed, true, true),
            (RecoveryPhase::ReplacePrepared, true, true),
            (RecoveryPhase::PublishPrepared, true, true),
            (RecoveryPhase::RemovePrepared, false, false),
            (RecoveryPhase::RemoveCommitted, true, true),
        ] {
            assert_eq!(phase.owns_target(), target, "target projection {phase:?}");
            assert_eq!(phase.owns_park(), park, "park projection {phase:?}");
        }
    }

    #[test]
    fn replacement_classifier_is_the_exact_canonical_cartesian() {
        const PHASES: [RecoveryPhase; 6] = [
            RecoveryPhase::StagePrepared,
            RecoveryPhase::StageSealed,
            RecoveryPhase::ReplacePrepared,
            RecoveryPhase::PublishPrepared,
            RecoveryPhase::RemovePrepared,
            RecoveryPhase::RemoveCommitted,
        ];
        const CARRIERS: [ReplacementCarrier; 7] = [
            ReplacementCarrier::Unobserved,
            ReplacementCarrier::Absent,
            ReplacementCarrier::Unsealed,
            ReplacementCarrier::Other,
            ReplacementCarrier::Old,
            ReplacementCarrier::New,
            ReplacementCarrier::OldAndNew,
        ];
        let cases = canonical_replacement_cases();
        let mut accepted = 0;
        let mut checked = 0;
        for proofs_equal in [false, true] {
            for phase in PHASES {
                if proofs_equal && phase == RecoveryPhase::StagePrepared {
                    continue;
                }
                let record = replacement_record(phase, proofs_equal);
                for stage in CARRIERS {
                    for target in CARRIERS {
                        for park in CARRIERS {
                            let expected = cases.iter().find_map(
                                |&(
                                    equal,
                                    candidate_phase,
                                    candidate_stage,
                                    candidate_target,
                                    candidate_park,
                                    action,
                                )| {
                                    (equal == proofs_equal
                                        && candidate_phase == phase
                                        && candidate_stage == stage
                                        && candidate_target == target
                                        && candidate_park == park)
                                        .then_some(action)
                                },
                            );
                            let actual = classify_replacement(&record, (stage, target, park));
                            assert_eq!(
                                actual, expected,
                                "classification {proofs_equal:?} {phase:?} ({stage:?}, {target:?}, {park:?})"
                            );
                            checked += 1;
                            accepted += usize::from(actual.is_some());
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 3_773);
        assert_eq!(accepted, cases.len());
        assert_eq!(accepted, 47);

        let create_only = record(RecoveryPhase::StageSealed);
        assert_eq!(
            classify_replacement(
                &create_only,
                (
                    ReplacementCarrier::New,
                    ReplacementCarrier::Old,
                    ReplacementCarrier::Absent,
                ),
            ),
            None
        );
        let mut invalid = replacement_record(RecoveryPhase::StageSealed, false);
        invalid.new = None;
        assert_eq!(
            classify_replacement(
                &invalid,
                (
                    ReplacementCarrier::New,
                    ReplacementCarrier::Old,
                    ReplacementCarrier::Absent,
                ),
            ),
            None
        );
    }

    #[test]
    fn equal_proofs_follow_phase_and_coordinate_roles() {
        use RecoveryPhase::*;
        use ReplacementAction::*;
        use ReplacementCarrier::{Absent as A, OldAndNew as B};

        for (phase, stage, target, park, action) in [
            (ReplacePrepared, B, B, A, ParkTarget),
            (PublishPrepared, B, B, A, Advance(RemovePrepared)),
            (PublishPrepared, A, B, A, Advance(RemoveCommitted)),
            (PublishPrepared, A, B, B, RemovePark),
        ] {
            let record = replacement_record(phase, true);
            assert_eq!(
                classify_replacement(&record, (stage, target, park)),
                Some(action)
            );
        }
    }

    #[test]
    fn replacement_phase_transition_graph_is_closed() {
        use RecoveryPhase::{
            PublishPrepared as PP, RemoveCommitted as RC, RemovePrepared as RmP,
            ReplacePrepared as RP, StagePrepared as SP, StageSealed as SS,
        };
        const PHASES: [RecoveryPhase; 6] = [SP, SS, RP, PP, RmP, RC];
        let allowed = [
            (SP, SS),
            (SS, RP),
            (SS, RmP),
            (RP, PP),
            (RP, RmP),
            (PP, RmP),
            (PP, RC),
        ];
        for proofs_equal in [false, true] {
            for previous_phase in PHASES {
                let previous = RecoveryFrame {
                    generation: 1,
                    record: Some(replacement_record(previous_phase, proofs_equal)),
                };
                for next_phase in PHASES {
                    let next = RecoveryFrame {
                        generation: 2,
                        record: Some(replacement_record(next_phase, proofs_equal)),
                    };
                    assert_eq!(
                        validate_recovery_advance(Some(&previous), &next).is_ok(),
                        allowed.contains(&(previous_phase, next_phase)),
                        "transition {previous_phase:?} -> {next_phase:?}"
                    );
                }
                let tombstone = RecoveryFrame {
                    generation: 2,
                    record: None,
                };
                assert_eq!(
                    validate_recovery_advance(Some(&previous), &tombstone).is_ok(),
                    previous_phase != SS,
                    "tombstone after {previous_phase:?}"
                );
            }
        }

        for phase in [SP, SS, PP, RmP, RC] {
            let previous = RecoveryFrame {
                generation: 1,
                record: Some(record(phase)),
            };
            let tombstone = RecoveryFrame {
                generation: 2,
                record: None,
            };
            assert_eq!(
                validate_recovery_advance(Some(&previous), &tombstone).is_ok(),
                phase != SS,
                "create-only tombstone after {phase:?}"
            );
        }
    }

    #[test]
    fn every_phase_enforces_its_exact_proof_fields() {
        for phase in [
            RecoveryPhase::StagePrepared,
            RecoveryPhase::StageSealed,
            RecoveryPhase::ReplacePrepared,
            RecoveryPhase::PublishPrepared,
            RecoveryPhase::RemovePrepared,
            RecoveryPhase::RemoveCommitted,
        ] {
            let valid = record(phase);
            assert!(valid.validate().is_ok(), "valid phase {phase:?}");
            for old in [None, Some(proof(1, 1))] {
                for new in [None, Some(proof(1, 2))] {
                    let mut candidate = valid.clone();
                    candidate.old = old;
                    candidate.new = new;
                    let expected = match phase {
                        RecoveryPhase::StagePrepared => new.is_none(),
                        RecoveryPhase::StageSealed => new.is_some(),
                        RecoveryPhase::ReplacePrepared => old.is_some() && new.is_some(),
                        RecoveryPhase::PublishPrepared
                        | RecoveryPhase::RemovePrepared
                        | RecoveryPhase::RemoveCommitted => new.is_some(),
                    };
                    assert_eq!(candidate.validate().is_ok(), expected, "phase {phase:?}");
                }
            }
        }
    }

    #[test]
    fn advances_preserve_mode_identity_coordinates_and_sealed_proof() {
        let mut prepared = record(RecoveryPhase::StagePrepared);
        prepared.old = Some(proof(5, 0x20));
        let first = RecoveryFrame {
            generation: 1,
            record: Some(prepared.clone()),
        };
        assert!(validate_recovery_advance(None, &first).is_ok());

        let mut sealed = prepared.clone();
        sealed.phase = RecoveryPhase::StageSealed;
        sealed.new = Some(proof(7, 0x31));
        let second = RecoveryFrame {
            generation: 2,
            record: Some(sealed.clone()),
        };
        assert!(validate_recovery_advance(Some(&first), &second).is_ok());
        let mut removal = sealed.clone();
        removal.phase = RecoveryPhase::RemovePrepared;
        let removal = RecoveryFrame {
            generation: 3,
            record: Some(removal),
        };
        assert!(validate_recovery_advance(Some(&second), &removal).is_ok());
        let mut changed_removal = removal.clone();
        changed_removal.record.as_mut().unwrap().new = Some(proof(7, 0x32));
        assert!(validate_recovery_advance(Some(&second), &changed_removal).is_err());

        let mut changed_old = sealed.clone();
        changed_old.phase = RecoveryPhase::ReplacePrepared;
        changed_old.old = Some(proof(5, 0x21));
        let mut changed_new = sealed.clone();
        changed_new.phase = RecoveryPhase::ReplacePrepared;
        changed_new.new = Some(proof(7, 0x32));
        let mut changed_target = sealed.clone();
        changed_target.phase = RecoveryPhase::ReplacePrepared;
        changed_target.destination_leaf = name("other.bin");
        let mut changed_operation = sealed.clone();
        changed_operation.phase = RecoveryPhase::ReplacePrepared;
        changed_operation.operation_id = [9; 16];
        for invalid in [changed_old, changed_new, changed_target, changed_operation] {
            let frame = RecoveryFrame {
                generation: 3,
                record: Some(invalid),
            };
            assert!(validate_recovery_advance(Some(&second), &frame).is_err());
        }

        let mut replacement = sealed;
        replacement.phase = RecoveryPhase::ReplacePrepared;
        let third = RecoveryFrame {
            generation: 3,
            record: Some(replacement),
        };
        assert!(validate_recovery_advance(Some(&second), &third).is_ok());
        let mut publish = third.record.clone().unwrap();
        publish.phase = RecoveryPhase::PublishPrepared;
        let publish = RecoveryFrame {
            generation: 4,
            record: Some(publish),
        };
        assert!(validate_recovery_advance(Some(&third), &publish).is_ok());
        let mut publish_removal = publish.record.clone().unwrap();
        publish_removal.phase = RecoveryPhase::RemovePrepared;
        let publish_removal = RecoveryFrame {
            generation: 5,
            record: Some(publish_removal),
        };
        assert!(validate_recovery_advance(Some(&publish), &publish_removal).is_ok());
        let tombstone = RecoveryFrame {
            generation: 6,
            record: None,
        };
        assert!(validate_recovery_advance(Some(&publish_removal), &tombstone).is_ok());
    }

    #[test]
    fn paths_enforce_component_name_file_and_portable_alias_bounds() {
        let mut too_many = record(RecoveryPhase::PublishPrepared);
        too_many.destination_parent = (0..32).map(|index| name(&format!("p{index}"))).collect();
        assert!(too_many.validate().is_err());

        for unsafe_name in [
            "",
            ".",
            "..",
            "NUL.txt",
            "trailing.",
            "a/b",
            "a\\b",
            "Cafe\u{301}",
            &"x".repeat(256),
        ] {
            assert!(
                RecoveryName::new_exact(unsafe_name).is_err(),
                "{unsafe_name:?}"
            );
        }
        assert!(RecoveryName::new_exact("😀".repeat(128)).is_err());

        let mut oversized = record(RecoveryPhase::PublishPrepared);
        oversized.new = Some(proof(MAX_RECOVERABLE_FILE_BYTES + 1, 3));
        assert!(oversized.validate().is_err());

        let mut alias = record(RecoveryPhase::PublishPrepared);
        alias.old = Some(proof(1, 4));
        alias.destination_leaf = recovery_park_leaf(alias.operation_id);
        assert!(alias.validate().is_err());

        let mut duplicate = record(RecoveryPhase::PublishPrepared);
        duplicate.destination_leaf = recovery_stage_leaf(duplicate.operation_id);
        assert!(duplicate.validate().is_err());

        let mut lease = record(RecoveryPhase::PublishPrepared);
        lease.destination_leaf = name(".AXIAL-ROOT.LEASE");
        lease.destination_parent.clear();
        assert!(lease.validate().is_err());
        lease.destination_parent.push(name("nested"));
        assert!(lease.validate().is_ok());
    }

    #[test]
    fn decoder_rejects_a_resigned_portable_coordinate_alias() {
        let mut value = record(RecoveryPhase::PublishPrepared);
        value.destination_parent.clear();
        value.destination_leaf = recovery_stage_leaf([0x12; 16]);
        let destination = value.destination_leaf.as_str().as_bytes().to_vec();
        let alias = recovery_stage_leaf(value.operation_id);
        let mut bytes = encode_recovery_frame(
            &RecoveryFrame {
                generation: 1,
                record: Some(value),
            },
            address(0),
        )
        .unwrap();
        let destination_start = bytes[..FRAME_BODY_BYTES]
            .windows(destination.len())
            .position(|window| window == destination)
            .expect("encoded destination");
        bytes[destination_start..destination_start + destination.len()]
            .copy_from_slice(alias.as_str().as_bytes());
        resign(&mut bytes, 0);
        assert_eq!(
            decode_recovery_frame(&bytes, address(0)),
            Err(RecoveryCodecError)
        );
    }

    #[test]
    fn failed_write_and_unconfirmed_sync_never_reload_cached_bytes_as_durable() {
        for failure in ["write", "sync"] {
            let mut journal = empty_journal();
            let prepared = record(RecoveryPhase::StagePrepared);
            let registration = journal.reserve(&prepared).unwrap();
            let reloads = Cell::new(0);
            let result = journal.write_with(
                registration,
                Some(prepared),
                |_, _| {
                    if failure == "write" {
                        Err(io::Error::other("injected write failure"))
                    } else {
                        Ok(())
                    }
                },
                || {
                    if failure == "sync" {
                        Err((io::Error::other("injected sync failure"), false))
                    } else {
                        panic!("sync must not follow a failed write")
                    }
                },
                |_, _| panic!("readback must not follow an unconfirmed barrier"),
                || {
                    reloads.set(reloads.get() + 1);
                    Ok(empty_journal())
                },
            );
            assert!(result.is_err(), "{failure} failure was accepted");
            assert!(journal.is_uncertain(), "{failure} failure stayed writable");
            assert_eq!(reloads.get(), 0, "{failure} failure consulted page cache");
        }
    }

    #[test]
    fn only_a_confirmed_barrier_can_reconcile_an_exact_intended_frame() {
        let prepared = record(RecoveryPhase::StagePrepared);
        let intended = RecoveryFrame {
            generation: 1,
            record: Some(prepared.clone()),
        };
        let mut journal = empty_journal();
        let registration = journal.reserve(&prepared).unwrap();
        journal
            .write_with(
                registration,
                Some(prepared),
                |_, _| Ok(()),
                || {
                    Err((
                        io::Error::other("injected post-sync validation failure"),
                        true,
                    ))
                },
                |_, _| panic!("readback is skipped after the injected sync result"),
                || Ok(journal_with(0, intended)),
            )
            .expect("confirmed exact frame should reconcile");
        assert!(!journal.is_uncertain());
        assert!(journal.record(registration).is_some());

        let prepared = record(RecoveryPhase::StagePrepared);
        let mut journal = empty_journal();
        let registration = journal.reserve(&prepared).unwrap();
        assert!(
            journal
                .write_with(
                    registration,
                    Some(prepared),
                    |_, _| Ok(()),
                    || Err((
                        io::Error::other("injected post-sync validation failure"),
                        true
                    )),
                    |_, _| panic!("readback is skipped after the injected sync result"),
                    || Ok(empty_journal()),
                )
                .is_err()
        );
        assert!(journal.is_uncertain());
    }

    #[test]
    fn ordinary_post_sync_readback_failure_reloads_the_exact_durable_frame() {
        let prepared = record(RecoveryPhase::StagePrepared);
        let intended = RecoveryFrame {
            generation: 1,
            record: Some(prepared.clone()),
        };
        let mut journal = empty_journal();
        let registration = journal.reserve(&prepared).unwrap();
        let readbacks = Cell::new(0);
        journal
            .write_with(
                registration,
                Some(prepared),
                |_, _| Ok(()),
                || Ok(()),
                |_, _| {
                    readbacks.set(readbacks.get() + 1);
                    Err(io::Error::other("injected ordinary readback failure"))
                },
                || Ok(journal_with(0, intended)),
            )
            .expect("exact durable reload should reconcile failed readback");
        assert_eq!(readbacks.get(), 1);
        assert!(!journal.is_uncertain());
        assert!(journal.record(registration).is_some());
    }

    #[test]
    fn uncertain_readback_accepts_only_the_exact_predecessor_or_intended_lane() {
        let prepared = record(RecoveryPhase::StagePrepared);
        let intended = RecoveryFrame {
            generation: 1,
            record: Some(prepared.clone()),
        };
        let registration = RecoveryRegistration {
            slot: 0,
            operation_id: prepared.operation_id,
        };
        let pending = UncertainRecoveryWrite {
            registration,
            predecessor: None,
            intended: intended.clone(),
        };
        let journal = empty_journal();
        let mut control = vec![0; usize::try_from(RECOVERY_CONTROL_BYTES).unwrap()];
        assert_eq!(
            validate_uncertain_control(&control, &journal, &pending),
            Ok(UncertainRecoverySelection::Predecessor)
        );

        let exact = encode_recovery_frame(
            &intended,
            RecoveryFrameAddress::new(LANE, 0, generation_side(intended.generation)).unwrap(),
        )
        .unwrap();
        control[..RECOVERY_FRAME_BYTES].copy_from_slice(&exact);
        assert_eq!(
            validate_uncertain_control(&control, &journal, &pending),
            Ok(UncertainRecoverySelection::Intended)
        );

        control[..RECOVERY_FRAME_BYTES].fill(0);
        control[..128].copy_from_slice(&exact[..128]);
        assert_eq!(
            validate_uncertain_control(&control, &journal, &pending),
            Ok(UncertainRecoverySelection::Predecessor),
            "a first-generation torn frame remains exactly rewritable"
        );

        let mut foreign = prepared;
        foreign.operation_id = [0x44; 16];
        foreign.destination_leaf = name("foreign.bin");
        let foreign = encode_recovery_frame(
            &RecoveryFrame {
                generation: 1,
                record: Some(foreign),
            },
            RecoveryFrameAddress::new(LANE, 1, 0).unwrap(),
        )
        .unwrap();
        let start = 2 * RECOVERY_FRAME_BYTES;
        control[start..start + RECOVERY_FRAME_BYTES].copy_from_slice(&foreign);
        assert_eq!(
            validate_uncertain_control(&control, &journal, &pending),
            Err(RecoveryCodecError),
            "an unrelated valid lane transition must not clear uncertainty"
        );
    }
}
