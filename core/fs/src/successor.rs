use crate::control_frame::{self, Cursor, Domain, Envelope, Probe, generation_side};
use std::collections::BTreeSet;
pub(crate) const SUCCESSOR_SLOT_COUNT: usize = 64;
const FRAMES_PER_SLOT: usize = 2;
pub(crate) const SUCCESSOR_FRAME_BYTES: usize = control_frame::FRAME_BYTES;
const HEADER_BYTES: usize = control_frame::HEADER_BYTES;
const FRAME_BODY_BYTES: usize = control_frame::BODY_BYTES;
const RECORD_HEADER_BYTES: usize = 48;
const ACKNOWLEDGEMENT_BYTES: usize = 64;
const MAX_OWNER_ID_BYTES: usize = 255;
const MAX_ACKNOWLEDGEMENTS: usize = 32;
const MAX_DOMAIN_PAYLOAD_BYTES: usize =
    FRAME_BODY_BYTES - HEADER_BYTES - RECORD_HEADER_BYTES - 1 - ACKNOWLEDGEMENT_BYTES;
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("successor frame is invalid")]
pub(crate) struct SuccessorCodecError;
type Result<T> = std::result::Result<T, SuccessorCodecError>;
impl From<control_frame::Error> for SuccessorCodecError {
    fn from(_: control_frame::Error) -> Self {
        Self
    }
}
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
    pub(super) recovery_slot: u8,
    pub(super) recovery_side: u8,
    pub(super) recovery_generation: u64,
    pub(super) operation_id: [u8; 16],
    pub(super) frame_sha256: [u8; 32],
}
impl SuccessorAcknowledgement {
    fn validate(&self) -> Result<()> {
        require(
            usize::from(self.recovery_slot) < SUCCESSOR_SLOT_COUNT
                && usize::from(self.recovery_side) < FRAMES_PER_SLOT
                && (1..(u64::MAX - 1)).contains(&self.recovery_generation)
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
        require(
            HEADER_BYTES
                + RECORD_HEADER_BYTES
                + self.owner_id.len()
                + self.acknowledgements.len() * ACKNOWLEDGEMENT_BYTES
                + old_len
                + new_len
                <= FRAME_BODY_BYTES,
        )
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
    pub(crate) frame: SuccessorFrame,
}
pub(super) enum FrameProbe {
    Empty,
    Torn,
    Valid(SelectedSuccessorFrame),
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
    control_frame::offset(Domain::Successor, slot, side).map_err(Into::into)
}
pub(crate) fn encode_successor_frame(
    frame: &SuccessorFrame,
    lane_nonce: [u8; 16],
    slot: u8,
) -> Result<[u8; SUCCESSOR_FRAME_BYTES]> {
    frame.validate()?;
    let side = generation_side(frame.generation);
    let mut payload = Vec::new();
    let kind = if let Some(record) = &frame.record {
        encode_record(record, &mut payload);
        1
    } else {
        0
    };
    Ok(control_frame::encode(
        Domain::Successor,
        control_frame::Address::new(lane_nonce, slot, side)?,
        frame.generation,
        kind,
        &payload,
    )?)
}
fn decode_envelope(envelope: Envelope<'_>, lane_nonce: [u8; 16]) -> Result<SuccessorFrame> {
    require(envelope.address.lane == lane_nonce)?;
    let record = match envelope.kind {
        0 if envelope.payload.is_empty() => None,
        1 => Some(decode_record(envelope.payload)?),
        _ => return Err(SuccessorCodecError),
    };
    let frame = SuccessorFrame {
        generation: envelope.generation,
        record,
    };
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
pub(super) fn probe_frame(encoded: &[u8], slot: u8, side: u8) -> Result<FrameProbe> {
    Ok(
        match control_frame::probe(Domain::Successor, encoded, slot, side)? {
            Probe::Empty => FrameProbe::Empty,
            Probe::Torn => FrameProbe::Torn,
            Probe::Valid(envelope) => {
                let lane_nonce = envelope.address.lane;
                FrameProbe::Valid(SelectedSuccessorFrame {
                    lane_nonce,
                    frame: decode_envelope(envelope, lane_nonce)?,
                })
            }
        },
    )
}
pub(crate) fn validate_successor_lane(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
    change: Option<(usize, &SuccessorFrame)>,
) -> Result<()> {
    require(lane_nonce != [0; 16] && slots.len() == SUCCESSOR_SLOT_COUNT)?;
    require(change.is_none_or(|(slot, _)| slot < slots.len()))?;
    let mut owners = BTreeSet::new();
    let mut transfers = BTreeSet::new();
    let mut recovery_slots = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut state_owner = false;
    for (slot, selected) in slots.iter().enumerate() {
        let frame = if change.is_some_and(|(changed, _)| changed == slot) {
            change.map(|(_, frame)| frame)
        } else {
            let Some(selected) = selected else { continue };
            require(selected.lane_nonce == lane_nonce)?;
            Some(&selected.frame)
        };
        let Some(frame) = frame else { continue };
        frame.validate()?;
        let Some(record) = &frame.record else {
            continue;
        };
        require(
            (record.owner_class != SuccessorOwnerClass::State
                || !std::mem::replace(&mut state_owner, true))
                && owners.insert((record.owner_class, record.owner_id.as_slice()))
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
pub(crate) fn recovery_pin_side(
    lane_nonce: [u8; 16],
    slots: &[Option<SelectedSuccessorFrame>],
    recovery_slot: u8,
) -> Result<Option<u8>> {
    validate_successor_lane(lane_nonce, slots, None)?;
    require(usize::from(recovery_slot) < SUCCESSOR_SLOT_COUNT)?;
    Ok(slots
        .iter()
        .filter_map(Option::as_ref)
        .filter_map(|selected| selected.frame.record.as_ref())
        .flat_map(|record| &record.acknowledgements)
        .find(|acknowledgement| acknowledgement.recovery_slot == recovery_slot)
        .map(|acknowledgement| acknowledgement.recovery_side))
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
    let mut cursor = Cursor(payload);
    let class = SuccessorOwnerClass::decode(cursor.take(1)?[0])?;
    let flags = cursor.take(1)?[0];
    require(flags & !0b11 == 0 && flags != 0)?;
    let schema = u16::from_le_bytes(cursor.array()?);
    let owner_len = usize::from(u16::from_le_bytes(cursor.array()?));
    let acknowledgement_count = usize::from(cursor.take(1)?[0]);
    require(cursor.take(1)?[0] == 0)?;
    let transfer_id = cursor.array::<16>()?;
    let old_len =
        usize::try_from(u32::from_le_bytes(cursor.array()?)).map_err(|_| SuccessorCodecError)?;
    let new_len =
        usize::try_from(u32::from_le_bytes(cursor.array()?)).map_err(|_| SuccessorCodecError)?;
    require(cursor.take(16)?.iter().all(|byte| *byte == 0))?;
    let id = cursor.take(owner_len)?.to_vec();
    let mut acknowledgements = Vec::with_capacity(acknowledgement_count);
    for _ in 0..acknowledgement_count {
        let recovery_slot = cursor.take(1)?[0];
        let recovery_side = cursor.take(1)?[0];
        require(cursor.take(6)?.iter().all(|byte| *byte == 0))?;
        acknowledgements.push(SuccessorAcknowledgement {
            recovery_slot,
            recovery_side,
            recovery_generation: u64::from_le_bytes(cursor.array()?),
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
    require(cursor.0.is_empty())?;
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
fn require(valid: bool) -> Result<()> {
    valid.then_some(()).ok_or(SuccessorCodecError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_frame::PLATFORM as PLATFORM_TAG;
    use sha2::{Digest as _, Sha256};

    const LANE: [u8; 16] = [0x21; 16];
    const RECOVERY_FRAME_BYTES: usize = 16 * 1024;
    const SUCCESSOR_REGION_OFFSET: u64 = 2 * 1024 * 1024;
    const SUCCESSOR_REGION_BYTES: u64 = 2 * 1024 * 1024;

    fn validate_successor_lane(
        lane: [u8; 16],
        slots: &[Option<SelectedSuccessorFrame>],
    ) -> Result<()> {
        super::validate_successor_lane(lane, slots, None)
    }

    fn decode_successor_frame(
        encoded: &[u8],
        lane: [u8; 16],
        slot: u8,
        side: u8,
    ) -> Result<SuccessorFrame> {
        let Probe::Valid(envelope) = control_frame::probe(Domain::Successor, encoded, slot, side)?
        else {
            return Err(SuccessorCodecError);
        };
        decode_envelope(envelope, lane)
    }

    fn frame_checksum(body: &[u8], side: u8) -> [u8; 32] {
        control_frame::checksum(Domain::Successor, body, side)
    }

    fn acknowledgement(slot: u8, operation: u8, _transfer: u8) -> SuccessorAcknowledgement {
        let frame = [operation.wrapping_add(1); RECOVERY_FRAME_BYTES];
        SuccessorAcknowledgement {
            recovery_slot: slot,
            recovery_side: 0,
            recovery_generation: 1,
            operation_id: [operation; 16],
            frame_sha256: Sha256::digest(frame).into(),
        }
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
        _slot: u8,
        generation: u64,
        record: Option<SuccessorRecord>,
    ) -> SelectedSuccessorFrame {
        SelectedSuccessorFrame {
            lane_nonce: LANE,
            frame: frame(generation, record),
        }
    }

    fn lane(
        entry: SelectedSuccessorFrame,
    ) -> [Option<SelectedSuccessorFrame>; SUCCESSOR_SLOT_COUNT] {
        let mut slots = std::array::from_fn(|_| None);
        slots[0] = Some(entry);
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

        for mutate in 0..6 {
            assert_invalid_record(|value| match mutate {
                0 => value.acknowledgements[0].recovery_slot = 64,
                1 => value.acknowledgements[0].recovery_side = 2,
                2 => value.acknowledgements[0].recovery_generation = 0,
                3 => value.acknowledgements[0].recovery_generation = u64::MAX,
                4 => value.acknowledgements[0].recovery_generation = u64::MAX - 1,
                5 => value.acknowledgements[0].operation_id = [0; 16],
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
        assert!(validate_successor_lane(LANE, &slots).is_err());

        slots[1] = Some(selected(
            1,
            1,
            Some(record(SuccessorOwnerClass::Performance, 2, 3)),
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
    fn recovery_tombstone_cannot_release_a_pin_before_the_successor_tombstone() {
        let record = record(SuccessorOwnerClass::State, 1, 2);
        let recovery_slot = record.acknowledgements[0].recovery_slot;
        let slots = lane(selected(0, 1, Some(record)));
        assert_eq!(recovery_pin_side(LANE, &slots, recovery_slot), Ok(Some(0)));
        assert_eq!(
            generation_side(2),
            1,
            "recovery tombstone uses the other side"
        );
        let tombstoned = lane(selected(0, 2, None));
        assert_eq!(
            recovery_pin_side(LANE, &tombstoned, recovery_slot),
            Ok(None)
        );
        assert!(recovery_pin_side(LANE, &slots, 64).is_err());
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
