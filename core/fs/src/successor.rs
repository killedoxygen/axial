use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
pub(crate) const SUCCESSOR_SLOT_COUNT: usize = 64;
const FRAMES_PER_SLOT: usize = 2;
pub(crate) const SUCCESSOR_FRAME_BYTES: usize = 16 * 1024;
pub(crate) const SUCCESSOR_REGION_OFFSET: u64 = 2 * 1024 * 1024;
const SUCCESSOR_REGION_BYTES: u64 =
    SUCCESSOR_SLOT_COUNT as u64 * FRAMES_PER_SLOT as u64 * SUCCESSOR_FRAME_BYTES as u64;
const MAGIC: &[u8; 8] = b"AXSUCC01";
const SCHEMA: u16 = 1;
const HEADER_BYTES: usize = 52;
const CHECKSUM_BYTES: usize = 32;
const FRAME_BODY_BYTES: usize = SUCCESSOR_FRAME_BYTES - CHECKSUM_BYTES;
const RECORD_HEADER_BYTES: usize = 48;
const ACKNOWLEDGEMENT_BYTES: usize = 64;
const MAX_OWNER_ID_BYTES: usize = 255;
const MAX_ACKNOWLEDGEMENTS: usize = 32;
const MAX_DOMAIN_PAYLOAD_BYTES: usize =
    FRAME_BODY_BYTES - HEADER_BYTES - RECORD_HEADER_BYTES - 1 - ACKNOWLEDGEMENT_BYTES;
const CHECKSUM_DOMAIN: &[u8] = b"axial.fs.successor-frame.v1\0";
const _: () = assert!(SUCCESSOR_REGION_BYTES == 2 * 1024 * 1024);
#[cfg(target_os = "linux")]
const PLATFORM_TAG: u8 = 1;
#[cfg(target_os = "macos")]
const PLATFORM_TAG: u8 = 2;
#[cfg(target_os = "windows")]
const PLATFORM_TAG: u8 = 3;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("successor frames require a supported Linux, macOS, or Windows target");
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("successor frame is invalid")]
pub(crate) struct SuccessorCodecError;
type Result<T> = std::result::Result<T, SuccessorCodecError>;
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum SuccessorOwnerClass {
    State = 1,
    Performance = 2,
    Minecraft = 3,
}
impl SuccessorOwnerClass {
    fn decode(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::State),
            2 => Ok(Self::Performance),
            3 => Ok(Self::Minecraft),
            _ => Err(SuccessorCodecError),
        }
    }
}
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SuccessorAcknowledgement {
    recovery_slot: u8,
    recovery_side: u8,
    recovery_generation: u64,
    operation_id: [u8; 16],
    frame_sha256: [u8; 32],
}
impl SuccessorAcknowledgement {
    fn validate(&self) -> Result<()> {
        require(
            usize::from(self.recovery_slot) < SUCCESSOR_SLOT_COUNT
                && usize::from(self.recovery_side) < FRAMES_PER_SLOT
                && (1..u64::MAX).contains(&self.recovery_generation)
                && self.recovery_side == generation_side(self.recovery_generation)
                && self.operation_id != [0; 16],
        )
    }
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SuccessorRecord {
    pub(crate) owner_class: SuccessorOwnerClass,
    pub(crate) owner_schema: u16,
    pub(crate) owner_id: Vec<u8>,
    pub(crate) transfer_id: [u8; 16],
    pub(crate) old_payload: Option<Vec<u8>>,
    pub(crate) new_payload: Option<Vec<u8>>,
    pub(crate) acknowledgements: Vec<SuccessorAcknowledgement>,
}
impl SuccessorRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            self.owner_schema != 0
                && !self.owner_id.is_empty()
                && self.owner_id.len() <= MAX_OWNER_ID_BYTES
                && self.transfer_id != [0; 16]
                && !matches!((&self.old_payload, &self.new_payload), (None, None))
                && (1..=MAX_ACKNOWLEDGEMENTS).contains(&self.acknowledgements.len()),
        )?;
        let old_len = self.old_payload.as_deref().map_or(0, <[u8]>::len);
        let new_len = self.new_payload.as_deref().map_or(0, <[u8]>::len);
        require(
            old_len <= MAX_DOMAIN_PAYLOAD_BYTES
                && new_len <= MAX_DOMAIN_PAYLOAD_BYTES
                && old_len.checked_add(new_len).ok_or(SuccessorCodecError)?
                    <= MAX_DOMAIN_PAYLOAD_BYTES,
        )?;
        let mut slots = BTreeSet::new();
        let mut operations = BTreeSet::new();
        for acknowledgement in &self.acknowledgements {
            acknowledgement.validate()?;
            require(
                slots.insert(acknowledgement.recovery_slot)
                    && operations.insert(acknowledgement.operation_id),
            )?;
        }
        require(HEADER_BYTES + encoded_record_len(self) <= FRAME_BODY_BYTES)
    }
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SuccessorFrame {
    pub(crate) generation: u64,
    /// `None` is the canonical durable tombstone that releases every recovery pin.
    pub(crate) record: Option<SuccessorRecord>,
}
impl SuccessorFrame {
    fn validate(&self) -> Result<()> {
        require(
            (1..u64::MAX).contains(&self.generation)
                && self.record.is_some() != self.generation.is_multiple_of(2),
        )?;
        if let Some(record) = &self.record {
            record.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SelectedSuccessorFrame {
    pub(crate) lane_nonce: [u8; 16],
    pub(crate) slot: u8,
    pub(crate) side: u8,
    pub(crate) frame: SuccessorFrame,
}
enum FrameProbe {
    Empty,
    Torn,
    Valid(SelectedSuccessorFrame),
}
/// Reserved for conversion from the recovery codec's opaque selected-frame receipt.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RecoveryFrameIdentity {
    lane_nonce: [u8; 16],
    transfer_id: [u8; 16],
    acknowledgement: SuccessorAcknowledgement,
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RecoveryPin {
    recovery_slot: u8,
    selected_side: u8,
}
pub(crate) fn next_successor_generation(current: Option<u64>) -> Result<u64> {
    match current {
        None => Ok(1),
        Some(0) => Err(SuccessorCodecError),
        Some(generation) => generation.checked_add(1).ok_or(SuccessorCodecError),
    }
}
pub(crate) fn validate_successor_advance(
    previous: Option<&SuccessorFrame>,
    next: &SuccessorFrame,
) -> Result<()> {
    next.validate()?;
    require(
        next.generation == next_successor_generation(previous.map(|frame| frame.generation))?
            && matches!(
                (
                    previous.and_then(|frame| frame.record.as_ref()),
                    &next.record
                ),
                (None, Some(_)) | (Some(_), None)
            ),
    )
}
pub(crate) fn successor_frame_offset(slot: u8, side: u8) -> Result<u64> {
    require(usize::from(slot) < SUCCESSOR_SLOT_COUNT && usize::from(side) < FRAMES_PER_SLOT)?;
    let ordinal = usize::from(slot) * FRAMES_PER_SLOT + usize::from(side);
    Ok(SUCCESSOR_REGION_OFFSET + (ordinal * SUCCESSOR_FRAME_BYTES) as u64)
}
pub(crate) fn encode_successor_frame(
    frame: &SuccessorFrame,
    lane_nonce: [u8; 16],
    slot: u8,
) -> Result<[u8; SUCCESSOR_FRAME_BYTES]> {
    frame.validate()?;
    let side = generation_side(frame.generation);
    validate_address(lane_nonce, slot, side)?;
    let mut payload = Vec::new();
    let kind = if let Some(record) = &frame.record {
        encode_record(record, &mut payload);
        1
    } else {
        0
    };
    require(HEADER_BYTES + payload.len() <= FRAME_BODY_BYTES)?;
    let mut encoded = [0; SUCCESSOR_FRAME_BYTES];
    let mut header = Vec::with_capacity(HEADER_BYTES);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&SCHEMA.to_le_bytes());
    header.push(PLATFORM_TAG);
    header.push(kind);
    header.extend_from_slice(&lane_nonce);
    header.push(slot);
    header.push(side);
    header.extend_from_slice(&[0; 6]);
    header.extend_from_slice(&frame.generation.to_le_bytes());
    header.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| SuccessorCodecError)?
            .to_le_bytes(),
    );
    header.extend_from_slice(&[0; 4]);
    debug_assert_eq!(header.len(), HEADER_BYTES);
    encoded[..HEADER_BYTES].copy_from_slice(&header);
    encoded[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(&payload);
    let checksum = frame_checksum(&encoded[..FRAME_BODY_BYTES], side);
    encoded[FRAME_BODY_BYTES..].copy_from_slice(&checksum);
    Ok(encoded)
}
pub(crate) fn decode_successor_frame(
    encoded: &[u8],
    lane_nonce: [u8; 16],
    slot: u8,
    side: u8,
) -> Result<SuccessorFrame> {
    validate_address(lane_nonce, slot, side)?;
    require(encoded.len() == SUCCESSOR_FRAME_BYTES)?;
    let (body, checksum) = encoded.split_at(FRAME_BODY_BYTES);
    require(checksum == frame_checksum(body, side))?;
    let mut cursor = Cursor { remaining: body };
    require(
        cursor.take(MAGIC.len())? == MAGIC
            && cursor.u16()? == SCHEMA
            && cursor.u8()? == PLATFORM_TAG,
    )?;
    let kind = cursor.u8()?;
    require(
        cursor.array::<16>()? == lane_nonce
            && cursor.u8()? == slot
            && cursor.u8()? == side
            && cursor.take(6)?.iter().all(|byte| *byte == 0),
    )?;
    let generation = cursor.u64()?;
    require(generation != 0 && side == generation_side(generation))?;
    let payload_len = usize::try_from(cursor.u32()?).map_err(|_| SuccessorCodecError)?;
    require(cursor.take(4)?.iter().all(|byte| *byte == 0))?;
    let payload = cursor.take(payload_len)?;
    require(cursor.remaining.iter().all(|byte| *byte == 0))?;
    let record = match kind {
        0 if payload.is_empty() => None,
        1 => Some(decode_record(payload)?),
        _ => return Err(SuccessorCodecError),
    };
    let frame = SuccessorFrame { generation, record };
    frame.validate()?;
    Ok(frame)
}
pub(crate) fn select_successor_frame(
    first: &[u8],
    second: &[u8],
    slot: u8,
) -> Result<Option<SelectedSuccessorFrame>> {
    let first = probe_frame(first, slot, 0)?;
    let second = probe_frame(second, slot, 1)?;
    match (first, second) {
        (FrameProbe::Empty, FrameProbe::Empty) => Ok(None),
        (FrameProbe::Valid(selected), FrameProbe::Torn)
        | (FrameProbe::Torn, FrameProbe::Valid(selected)) => Ok(Some(selected)),
        (FrameProbe::Valid(selected), FrameProbe::Empty)
        | (FrameProbe::Empty, FrameProbe::Valid(selected)) => {
            require(selected.frame.generation == 1 && selected.frame.record.is_some())?;
            Ok(Some(selected))
        }
        (FrameProbe::Valid(first), FrameProbe::Valid(second)) => {
            require(first.lane_nonce == second.lane_nonce)?;
            let (previous, next) = if first.frame.generation < second.frame.generation {
                (&first, second)
            } else if second.frame.generation < first.frame.generation {
                (&second, first)
            } else {
                return Err(SuccessorCodecError);
            };
            validate_successor_advance(Some(&previous.frame), &next.frame)?;
            Ok(Some(next))
        }
        (FrameProbe::Torn, FrameProbe::Empty | FrameProbe::Torn)
        | (FrameProbe::Empty, FrameProbe::Torn) => Err(SuccessorCodecError),
    }
}
fn probe_frame(encoded: &[u8], slot: u8, side: u8) -> Result<FrameProbe> {
    require(encoded.len() == SUCCESSOR_FRAME_BYTES)?;
    if encoded.iter().all(|byte| *byte == 0) {
        return Ok(FrameProbe::Empty);
    }
    let (body, checksum) = encoded.split_at(FRAME_BODY_BYTES);
    let declared_slot = encoded[28];
    let declared_side = encoded[29];
    if checksum != frame_checksum(body, declared_side) {
        return Ok(FrameProbe::Torn);
    }
    require(declared_slot == slot && declared_side == side)?;
    let lane_nonce = encoded[12..28]
        .try_into()
        .map_err(|_| SuccessorCodecError)?;
    let frame = decode_successor_frame(encoded, lane_nonce, slot, side)?;
    Ok(FrameProbe::Valid(SelectedSuccessorFrame {
        lane_nonce,
        slot,
        side,
        frame,
    }))
}
pub(crate) fn validate_successor_lane(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
) -> Result<()> {
    require(lane_nonce != [0; 16] && slots.len() == SUCCESSOR_SLOT_COUNT)?;
    let mut owners = BTreeSet::new();
    let mut transfers = BTreeSet::new();
    let mut recovery_slots = BTreeSet::new();
    let mut operations = BTreeSet::new();
    for (slot, selected) in slots.iter().enumerate() {
        let Some(selected) = selected else { continue };
        require(
            selected.lane_nonce == lane_nonce
                && usize::from(selected.slot) == slot
                && selected.side == generation_side(selected.frame.generation)
                && selected.frame.generation != u64::MAX,
        )?;
        selected.frame.validate()?;
        let Some(record) = &selected.frame.record else {
            continue;
        };
        require(
            owners.insert((record.owner_class, record.owner_id.as_slice()))
                && transfers.insert(record.transfer_id),
        )?;
        for acknowledgement in &record.acknowledgements {
            require(
                recovery_slots.insert(acknowledgement.recovery_slot)
                    && operations.insert(acknowledgement.operation_id),
            )?;
        }
    }
    Ok(())
}
pub(crate) fn validate_recovery_bindings(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
    mut recovery_frames: Vec<RecoveryFrameIdentity>,
) -> Result<()> {
    validate_successor_lane(lane_nonce, slots)?;
    for identity in &recovery_frames {
        identity.acknowledgement.validate()?;
        require(identity.lane_nonce == lane_nonce && identity.transfer_id != [0; 16])?;
    }
    for record in slots
        .iter()
        .filter_map(Option::as_ref)
        .filter_map(|selected| selected.frame.record.as_ref())
    {
        for acknowledgement in &record.acknowledgements {
            let exact = recovery_frames.iter().position(|identity| {
                identity.transfer_id == record.transfer_id
                    && &identity.acknowledgement == acknowledgement
            });
            require(exact.is_some())?;
            recovery_frames.swap_remove(exact.expect("checked exact recovery identity"));
        }
    }
    require(recovery_frames.is_empty())
}
/// A pin names its physical side; while present, the whole recovery slot is not reusable.
pub(crate) fn recovery_pin(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
    recovery_slot: u8,
) -> Result<Option<RecoveryPin>> {
    validate_successor_lane(lane_nonce, slots)?;
    require(usize::from(recovery_slot) < SUCCESSOR_SLOT_COUNT)?;
    Ok(slots
        .iter()
        .filter_map(Option::as_ref)
        .filter_map(|selected| selected.frame.record.as_ref())
        .flat_map(|record| &record.acknowledgements)
        .find(|acknowledgement| acknowledgement.recovery_slot == recovery_slot)
        .map(|acknowledgement| RecoveryPin {
            recovery_slot,
            selected_side: acknowledgement.recovery_side,
        }))
}
pub(crate) fn recovery_write_is_admissible(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
    recovery_slot: u8,
    selected_side: u8,
    write_side: u8,
    writes_tombstone: bool,
) -> Result<bool> {
    require(
        usize::from(selected_side) < FRAMES_PER_SLOT && usize::from(write_side) < FRAMES_PER_SLOT,
    )?;
    let Some(pin) = recovery_pin(lane_nonce, slots, recovery_slot)? else {
        return Ok(true);
    };
    debug_assert_eq!(pin.recovery_slot, recovery_slot);
    Ok(writes_tombstone && selected_side == pin.selected_side && write_side != pin.selected_side)
}
fn validate_address(lane_nonce: [u8; 16], slot: u8, side: u8) -> Result<()> {
    require(
        lane_nonce != [0; 16]
            && usize::from(slot) < SUCCESSOR_SLOT_COUNT
            && usize::from(side) < FRAMES_PER_SLOT,
    )
}
fn generation_side(generation: u64) -> u8 {
    u8::from(generation.is_multiple_of(2))
}
fn encoded_record_len(record: &SuccessorRecord) -> usize {
    RECORD_HEADER_BYTES
        + record.owner_id.len()
        + record.acknowledgements.len() * ACKNOWLEDGEMENT_BYTES
        + record.old_payload.as_deref().map_or(0, <[u8]>::len)
        + record.new_payload.as_deref().map_or(0, <[u8]>::len)
}
fn encode_record(record: &SuccessorRecord, output: &mut Vec<u8>) {
    output.push(record.owner_class as u8);
    let flags =
        u8::from(record.old_payload.is_some()) | (u8::from(record.new_payload.is_some()) << 1);
    output.push(flags);
    output.extend_from_slice(&record.owner_schema.to_le_bytes());
    output.extend_from_slice(
        &u16::try_from(record.owner_id.len())
            .expect("validated successor owner id fits u16")
            .to_le_bytes(),
    );
    output.push(
        u8::try_from(record.acknowledgements.len())
            .expect("validated successor acknowledgement count fits u8"),
    );
    output.push(0);
    output.extend_from_slice(&record.transfer_id);
    for payload in [&record.old_payload, &record.new_payload] {
        output.extend_from_slice(
            &u32::try_from(payload.as_deref().map_or(0, <[u8]>::len))
                .expect("validated successor payload length fits u32")
                .to_le_bytes(),
        );
    }
    output.extend_from_slice(&[0; 16]);
    debug_assert_eq!(output.len(), RECORD_HEADER_BYTES);
    output.extend_from_slice(&record.owner_id);
    for acknowledgement in &record.acknowledgements {
        output.push(acknowledgement.recovery_slot);
        output.push(acknowledgement.recovery_side);
        output.extend_from_slice(&[0; 6]);
        output.extend_from_slice(&acknowledgement.recovery_generation.to_le_bytes());
        output.extend_from_slice(&acknowledgement.operation_id);
        output.extend_from_slice(&acknowledgement.frame_sha256);
    }
    if let Some(payload) = &record.old_payload {
        output.extend_from_slice(payload);
    }
    if let Some(payload) = &record.new_payload {
        output.extend_from_slice(payload);
    }
}

fn decode_record(payload: &[u8]) -> Result<SuccessorRecord> {
    let mut cursor = Cursor { remaining: payload };
    let class = SuccessorOwnerClass::decode(cursor.u8()?)?;
    let flags = cursor.u8()?;
    require(flags & !0b11 == 0 && flags != 0)?;
    let schema = cursor.u16()?;
    let owner_len = usize::from(cursor.u16()?);
    let acknowledgement_count = usize::from(cursor.u8()?);
    require(cursor.u8()? == 0)?;
    let transfer_id = cursor.array::<16>()?;
    let old_len = usize::try_from(cursor.u32()?).map_err(|_| SuccessorCodecError)?;
    let new_len = usize::try_from(cursor.u32()?).map_err(|_| SuccessorCodecError)?;
    require(cursor.take(16)?.iter().all(|byte| *byte == 0))?;
    let id = cursor.take(owner_len)?.to_vec();
    let mut acknowledgements = Vec::with_capacity(acknowledgement_count);
    for _ in 0..acknowledgement_count {
        let recovery_slot = cursor.u8()?;
        let recovery_side = cursor.u8()?;
        require(cursor.take(6)?.iter().all(|byte| *byte == 0))?;
        acknowledgements.push(SuccessorAcknowledgement {
            recovery_slot,
            recovery_side,
            recovery_generation: cursor.u64()?,
            operation_id: cursor.array()?,
            frame_sha256: cursor.array()?,
        });
    }
    let old_payload = if flags & 1 != 0 {
        Some(cursor.take(old_len)?.to_vec())
    } else if old_len == 0 {
        None
    } else {
        return Err(SuccessorCodecError);
    };
    let new_payload = if flags & 2 != 0 {
        Some(cursor.take(new_len)?.to_vec())
    } else if new_len == 0 {
        None
    } else {
        return Err(SuccessorCodecError);
    };
    require(cursor.remaining.is_empty())?;
    let record = SuccessorRecord {
        owner_class: class,
        owner_schema: schema,
        owner_id: id,
        transfer_id,
        old_payload,
        new_payload,
        acknowledgements,
    };
    record.validate()?;
    Ok(record)
}

fn frame_checksum(body: &[u8], side: u8) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHECKSUM_DOMAIN);
    hasher.update([side]);
    hasher.update(body);
    hasher.finalize().into()
}

fn require(valid: bool) -> Result<()> {
    valid.then_some(()).ok_or(SuccessorCodecError)
}

struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(len)
            .ok_or(SuccessorCodecError)?;
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
        self.take(N)?.try_into().map_err(|_| SuccessorCodecError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANE: [u8; 16] = [0x21; 16];
    const RECOVERY_FRAME_BYTES: usize = 16 * 1024;

    fn identity(slot: u8, operation: u8, transfer: u8) -> RecoveryFrameIdentity {
        let frame = [operation.wrapping_add(1); RECOVERY_FRAME_BYTES];
        RecoveryFrameIdentity {
            lane_nonce: LANE,
            transfer_id: [transfer; 16],
            acknowledgement: SuccessorAcknowledgement {
                recovery_slot: slot,
                recovery_side: 0,
                recovery_generation: 1,
                operation_id: [operation; 16],
                frame_sha256: Sha256::digest(frame).into(),
            },
        }
    }

    fn acknowledgement(slot: u8, operation: u8, transfer: u8) -> SuccessorAcknowledgement {
        identity(slot, operation, transfer).acknowledgement
    }

    fn record(class: SuccessorOwnerClass, owner: u8, transfer: u8) -> SuccessorRecord {
        SuccessorRecord {
            owner_class: class,
            owner_schema: 1,
            owner_id: vec![owner],
            transfer_id: [transfer; 16],
            old_payload: Some(vec![1, 2]),
            new_payload: Some(vec![3, 4]),
            acknowledgements: vec![acknowledgement(owner, owner.wrapping_add(0x40), transfer)],
        }
    }

    fn assert_invalid_record(mutate: impl FnOnce(&mut SuccessorRecord)) {
        let mut value = record(SuccessorOwnerClass::State, 1, 2);
        mutate(&mut value);
        assert!(value.validate().is_err());
    }

    fn frame(generation: u64, record: Option<SuccessorRecord>) -> SuccessorFrame {
        SuccessorFrame { generation, record }
    }

    fn encoded(generation: u64, record: Option<SuccessorRecord>) -> [u8; SUCCESSOR_FRAME_BYTES] {
        encode_successor_frame(&frame(generation, record), LANE, 7).unwrap()
    }

    fn selected(
        slot: u8,
        generation: u64,
        record: Option<SuccessorRecord>,
    ) -> SelectedSuccessorFrame {
        SelectedSuccessorFrame {
            lane_nonce: LANE,
            slot,
            side: generation_side(generation),
            frame: frame(generation, record),
        }
    }

    fn lane(
        entry: SelectedSuccessorFrame,
    ) -> [Option<SelectedSuccessorFrame>; SUCCESSOR_SLOT_COUNT] {
        let mut slots = std::array::from_fn(|_| None);
        let slot = usize::from(entry.slot);
        slots[slot] = Some(entry);
        slots
    }

    fn resign(bytes: &mut [u8; SUCCESSOR_FRAME_BYTES], side: u8) {
        let checksum = frame_checksum(&bytes[..FRAME_BODY_BYTES], side);
        bytes[FRAME_BODY_BYTES..].copy_from_slice(&checksum);
    }

    #[test]
    fn live_tombstone_and_maximum_payload_round_trip_canonically() {
        let live = frame(1, Some(record(SuccessorOwnerClass::State, 1, 2)));
        let bytes = encode_successor_frame(&live, LANE, 7).unwrap();
        assert_eq!(decode_successor_frame(&bytes, LANE, 7, 0), Ok(live));

        let tombstone = frame(2, None);
        let bytes = encode_successor_frame(&tombstone, LANE, 7).unwrap();
        assert_eq!(decode_successor_frame(&bytes, LANE, 7, 1), Ok(tombstone));
        assert!(
            bytes[HEADER_BYTES..FRAME_BODY_BYTES]
                .iter()
                .all(|byte| *byte == 0)
        );

        let mut maximum = record(SuccessorOwnerClass::Minecraft, 1, 3);
        maximum.old_payload = Some(vec![0xa5; MAX_DOMAIN_PAYLOAD_BYTES]);
        maximum.new_payload = None;
        let bytes = encoded(1, Some(maximum));
        let decoded = decode_successor_frame(&bytes, LANE, 7, 0).unwrap();
        assert_eq!(
            decoded.record.unwrap().old_payload.unwrap().len(),
            MAX_DOMAIN_PAYLOAD_BYTES
        );
        assert_ne!(bytes[FRAME_BODY_BYTES - 1], 0);
    }

    #[test]
    fn decoder_rejects_every_truncation_trailing_byte_and_corruption_class() {
        let original = encoded(1, Some(record(SuccessorOwnerClass::Performance, 1, 2)));
        for len in 0..SUCCESSOR_FRAME_BYTES {
            assert_eq!(
                decode_successor_frame(&original[..len], LANE, 7, 0),
                Err(SuccessorCodecError),
                "accepted truncation at {len}"
            );
        }
        let mut trailing = original.to_vec();
        trailing.push(0);
        assert_eq!(
            decode_successor_frame(&trailing, LANE, 7, 0),
            Err(SuccessorCodecError)
        );
        for offset in [0, 17, HEADER_BYTES + 3, 1_024, FRAME_BODY_BYTES] {
            let mut corrupt = original;
            corrupt[offset] ^= 1;
            assert_eq!(
                decode_successor_frame(&corrupt, LANE, 7, 0),
                Err(SuccessorCodecError),
                "accepted corruption at {offset}"
            );
        }
    }

    #[test]
    fn decoder_rejects_resigned_noncanonical_header_record_and_padding() {
        let original = encoded(1, Some(record(SuccessorOwnerClass::State, 1, 2)));
        let payload_len = u32::from_le_bytes(original[44..48].try_into().unwrap());
        let mut cases = Vec::new();
        for (offset, value) in [
            (8, 2),
            (10, PLATFORM_TAG.wrapping_add(1)),
            (11, 7),
            (12, 0),
            (28, 8),
            (29, 1),
            (30, 1),
            (36, 0),
            (48, 1),
            (HEADER_BYTES, 7),
            (HEADER_BYTES + 1, 0x80),
            (HEADER_BYTES + 2, 0),
            (HEADER_BYTES + 4, 0),
            (HEADER_BYTES + 6, 0),
            (HEADER_BYTES + 7, 1),
            (HEADER_BYTES + 32, 1),
            (HEADER_BYTES + RECORD_HEADER_BYTES + 3, 1),
        ] {
            let mut changed = original;
            changed[offset] = value;
            cases.push(changed);
        }
        for length in [payload_len - 1, payload_len + 1, u32::MAX] {
            let mut changed = original;
            changed[44..48].copy_from_slice(&length.to_le_bytes());
            cases.push(changed);
        }
        let mut padding = original;
        padding[1_024] = 1;
        cases.push(padding);
        for mut changed in cases {
            resign(&mut changed, 0);
            assert_eq!(
                decode_successor_frame(&changed, LANE, 7, 0),
                Err(SuccessorCodecError)
            );
        }
        assert!(encode_successor_frame(&frame(1, None), [0; 16], 7).is_err());
        assert!(successor_frame_offset(64, 0).is_err());
        assert!(successor_frame_offset(0, 2).is_err());
    }

    #[test]
    fn selection_falls_back_from_a_torn_frame_and_rejects_impossible_histories() {
        let first = encoded(1, Some(record(SuccessorOwnerClass::State, 1, 2)));
        let second = encoded(2, None);
        assert_eq!(
            select_successor_frame(&first, &second, 7)
                .unwrap()
                .unwrap()
                .frame
                .generation,
            2
        );
        let mut torn = second;
        torn[HEADER_BYTES] ^= 1;
        assert_eq!(
            select_successor_frame(&first, &torn, 7)
                .unwrap()
                .unwrap()
                .frame
                .generation,
            1
        );
        let mut incomplete = [0; SUCCESSOR_FRAME_BYTES];
        incomplete[..128].copy_from_slice(&second[..128]);
        assert_eq!(
            select_successor_frame(&first, &incomplete, 7)
                .unwrap()
                .unwrap()
                .frame
                .generation,
            1
        );
        let mut torn_first = first;
        torn_first[HEADER_BYTES] ^= 1;
        assert_eq!(
            select_successor_frame(&torn_first, &second, 7)
                .unwrap()
                .unwrap()
                .frame
                .generation,
            2
        );

        let zero = [0; SUCCESSOR_FRAME_BYTES];
        assert!(select_successor_frame(&first, &zero, 7).is_ok());
        assert_eq!(
            select_successor_frame(&torn, &zero, 7),
            Err(SuccessorCodecError)
        );
        assert_eq!(select_successor_frame(&zero, &zero, 7), Ok(None));
        let gap = encoded(4, None);
        assert_eq!(
            select_successor_frame(&first, &gap, 7),
            Err(SuccessorCodecError)
        );
        assert!(
            frame(2, Some(record(SuccessorOwnerClass::Performance, 2, 3)))
                .validate()
                .is_err()
        );
        for (offset, value) in [
            (8, 2),
            (10, PLATFORM_TAG.wrapping_add(1)),
            (11, 1),
            (28, 8),
            (36, 3),
            (48, 1),
        ] {
            let mut checksum_valid_invalid = second;
            checksum_valid_invalid[offset] = value;
            resign(&mut checksum_valid_invalid, 1);
            assert_eq!(
                select_successor_frame(&first, &checksum_valid_invalid, 7),
                Err(SuccessorCodecError),
                "accepted checksum-valid semantic error at {offset}"
            );
        }
        assert_eq!(
            select_successor_frame(&first, &first, 7),
            Err(SuccessorCodecError),
            "a complete side-zero frame copied to side one is fatal"
        );
        let wrong_slot = encode_successor_frame(&frame(2, None), LANE, 8).unwrap();
        assert_eq!(
            select_successor_frame(&first, &wrong_slot, 7),
            Err(SuccessorCodecError),
            "a complete frame copied from another slot is fatal"
        );
        let foreign = encode_successor_frame(&frame(2, None), [0x33; 16], 7).unwrap();
        assert_eq!(
            select_successor_frame(&first, &foreign, 7),
            Err(SuccessorCodecError)
        );
    }

    #[test]
    fn record_bounds_and_acknowledgement_canonicality_fail_closed() {
        assert!(record(SuccessorOwnerClass::State, 1, 2).validate().is_ok());
        let mut exact_owner_bound = record(SuccessorOwnerClass::State, 1, 2);
        exact_owner_bound.owner_id = vec![0xff; MAX_OWNER_ID_BYTES];
        assert!(exact_owner_bound.validate().is_ok());
        let mut exact_ack_bound = record(SuccessorOwnerClass::State, 1, 2);
        exact_ack_bound.acknowledgements = (0..MAX_ACKNOWLEDGEMENTS)
            .map(|index| acknowledgement(u8::try_from(index).unwrap(), index as u8 + 1, 2))
            .collect();
        assert!(exact_ack_bound.validate().is_ok());
        assert_invalid_record(|value| value.owner_schema = 0);
        assert_invalid_record(|value| value.owner_id.clear());
        assert_invalid_record(|value| value.owner_id = vec![0; MAX_OWNER_ID_BYTES + 1]);
        assert_invalid_record(|value| value.transfer_id = [0; 16]);
        assert_invalid_record(|value| {
            value.old_payload = None;
            value.new_payload = None;
        });
        assert_invalid_record(|value| value.acknowledgements.clear());
        assert_invalid_record(|value| {
            value.acknowledgements.push(acknowledgement(1, 0x41, 2));
        });
        assert_invalid_record(|value| {
            value.acknowledgements.push(acknowledgement(1, 0x66, 2));
        });
        assert_invalid_record(|value| {
            value.acknowledgements.push(acknowledgement(2, 0x41, 2));
        });
        assert_invalid_record(|value| {
            value.acknowledgements = (0..=MAX_ACKNOWLEDGEMENTS)
                .map(|index| acknowledgement(u8::try_from(index).unwrap(), index as u8 + 1, 2))
                .collect();
        });
        assert_invalid_record(|value| {
            value.old_payload = Some(vec![0; MAX_DOMAIN_PAYLOAD_BYTES + 1]);
        });

        for mutate in 0..5 {
            assert_invalid_record(|value| match mutate {
                0 => value.acknowledgements[0].recovery_slot = 64,
                1 => value.acknowledgements[0].recovery_side = 2,
                2 => value.acknowledgements[0].recovery_generation = 0,
                3 => value.acknowledgements[0].recovery_generation = u64::MAX,
                4 => value.acknowledgements[0].operation_id = [0; 16],
                _ => unreachable!(),
            });
        }
    }

    #[test]
    fn duplicate_owners_transfers_acknowledgements_and_overlaps_fail_lane() {
        let mut slots = lane(selected(
            0,
            1,
            Some(record(SuccessorOwnerClass::State, 1, 2)),
        ));
        slots[1] = Some(selected(
            1,
            1,
            Some(record(SuccessorOwnerClass::State, 2, 3)),
        ));
        assert!(validate_successor_lane(LANE, &slots).is_ok());

        let mut duplicate_owner = record(SuccessorOwnerClass::State, 1, 4);
        duplicate_owner.owner_schema = 2;
        duplicate_owner.acknowledgements[0] = acknowledgement(2, 0x55, 4);
        slots[1] = Some(selected(1, 1, Some(duplicate_owner)));
        assert!(validate_successor_lane(LANE, &slots).is_err());

        let mut duplicate_transfer = record(SuccessorOwnerClass::Performance, 2, 2);
        duplicate_transfer.acknowledgements[0] = acknowledgement(2, 0x55, 2);
        slots[1] = Some(selected(1, 1, Some(duplicate_transfer)));
        assert!(validate_successor_lane(LANE, &slots).is_err());

        let mut overlap = record(SuccessorOwnerClass::Performance, 2, 4);
        overlap.acknowledgements[0].recovery_slot = 1;
        overlap.acknowledgements[0].operation_id = [0x77; 16];
        slots[1] = Some(selected(1, 1, Some(overlap)));
        assert!(validate_successor_lane(LANE, &slots).is_err());

        let mut duplicate_operation = record(SuccessorOwnerClass::Performance, 2, 4);
        duplicate_operation.acknowledgements[0].recovery_slot = 2;
        duplicate_operation.acknowledgements[0].operation_id = [0x41; 16];
        slots[1] = Some(selected(1, 1, Some(duplicate_operation)));
        assert!(validate_successor_lane(LANE, &slots).is_err());

        slots[1] = Some(selected(
            1,
            2,
            Some(record(SuccessorOwnerClass::Performance, 2, 4)),
        ));
        assert!(validate_successor_lane(LANE, &slots).is_err());
        slots[1] = Some(selected(1, 3, None));
        assert!(validate_successor_lane(LANE, &slots).is_err());
        slots[1] = Some(selected(1, u64::MAX, None));
        assert!(validate_successor_lane(LANE, &slots).is_err());
        assert!(validate_successor_lane([0x22; 16], &slots).is_err());
        assert!(validate_successor_lane(LANE, &slots[..63]).is_err());
    }

    #[test]
    fn exact_recovery_binding_rejects_stale_cross_operation_and_duplicate_receipts() {
        let slots = lane(selected(
            0,
            1,
            Some(record(SuccessorOwnerClass::State, 1, 2)),
        ));
        assert!(validate_recovery_bindings(LANE, &slots, vec![identity(1, 0x41, 2)]).is_ok());
        let full_frame = [0x42; RECOVERY_FRAME_BYTES];
        let exact = identity(1, 0x41, 2);
        assert_eq!(
            exact.acknowledgement.frame_sha256,
            <[u8; 32]>::from(Sha256::digest(full_frame))
        );
        for drift in 0..7 {
            let mut invalid = identity(1, 0x41, 2);
            match drift {
                0 => invalid.lane_nonce = [3; 16],
                1 => invalid.transfer_id = [3; 16],
                2 => invalid.acknowledgement.recovery_slot = 3,
                3 => {
                    invalid.acknowledgement.recovery_side = 1;
                    invalid.acknowledgement.recovery_generation = 2;
                }
                4 => invalid.acknowledgement.recovery_generation = 3,
                5 => invalid.acknowledgement.operation_id = [3; 16],
                6 => invalid.acknowledgement.frame_sha256[0] ^= 1,
                _ => unreachable!(),
            }
            assert!(validate_recovery_bindings(LANE, &slots, vec![invalid]).is_err());
        }
        assert!(
            validate_recovery_bindings(
                LANE,
                &slots,
                vec![identity(1, 0x41, 2), identity(1, 0x41, 2)],
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_tombstone_cannot_release_a_pin_before_the_successor_tombstone() {
        let record = record(SuccessorOwnerClass::State, 1, 2);
        let recovery_slot = record.acknowledgements[0].recovery_slot;
        let slots = lane(selected(0, 1, Some(record)));
        let pin = recovery_pin(LANE, &slots, recovery_slot).unwrap().unwrap();
        assert_eq!((pin.recovery_slot, pin.selected_side), (recovery_slot, 0));
        assert_eq!(
            generation_side(2),
            1,
            "recovery tombstone uses the other side"
        );
        assert_eq!(
            recovery_write_is_admissible(LANE, &slots, recovery_slot, 0, 1, true),
            Ok(true)
        );
        assert_eq!(
            recovery_write_is_admissible(LANE, &slots, recovery_slot, 1, 1, true),
            Ok(false),
            "only the acknowledged selected side can retire"
        );
        assert_eq!(
            recovery_write_is_admissible(LANE, &slots, recovery_slot, 1, 0, false),
            Ok(false),
            "a selected recovery tombstone does not admit reuse"
        );
        assert_eq!(
            recovery_write_is_admissible(LANE, &slots, recovery_slot, 0, 0, true),
            Ok(false),
            "the pinned physical side cannot be overwritten"
        );

        let tombstoned = lane(selected(0, 2, None));
        assert_eq!(recovery_pin(LANE, &tombstoned, recovery_slot), Ok(None));
        assert_eq!(
            recovery_write_is_admissible(LANE, &tombstoned, recovery_slot, 1, 0, false),
            Ok(true)
        );
        assert!(recovery_pin(LANE, &slots, 64).is_err());
    }

    #[test]
    fn generation_and_offsets_cover_exactly_the_second_control_half() {
        assert_eq!(next_successor_generation(None), Ok(1));
        assert_eq!(next_successor_generation(Some(41)), Ok(42));
        assert_eq!(next_successor_generation(Some(0)), Err(SuccessorCodecError));
        assert_eq!(
            next_successor_generation(Some(u64::MAX)),
            Err(SuccessorCodecError)
        );
        assert_eq!(successor_frame_offset(0, 0), Ok(SUCCESSOR_REGION_OFFSET));
        assert_eq!(
            successor_frame_offset(63, 1),
            Ok(SUCCESSOR_REGION_OFFSET + SUCCESSOR_REGION_BYTES - SUCCESSOR_FRAME_BYTES as u64)
        );
        assert_eq!(
            SUCCESSOR_REGION_OFFSET + SUCCESSOR_REGION_BYTES,
            4 * 1024 * 1024
        );

        let live = frame(1, Some(record(SuccessorOwnerClass::State, 1, 2)));
        let tombstone = frame(2, None);
        let reused = frame(3, Some(record(SuccessorOwnerClass::State, 3, 4)));
        assert!(validate_successor_advance(None, &live).is_ok());
        assert!(validate_successor_advance(Some(&live), &tombstone).is_ok());
        assert!(validate_successor_advance(Some(&tombstone), &reused).is_ok());
        assert!(validate_successor_advance(Some(&live), &reused).is_err());
        assert!(validate_successor_advance(Some(&tombstone), &frame(3, None)).is_err());
        let penultimate = frame(u64::MAX - 1, None);
        let unretirable = frame(u64::MAX, Some(record(SuccessorOwnerClass::State, 5, 6)));
        assert!(validate_successor_advance(Some(&penultimate), &unretirable).is_err());
        assert!(encode_successor_frame(&unretirable, LANE, 7).is_err());
    }
}
