use sha2::{Digest as _, Sha256};

pub(crate) const FRAME_BYTES: usize = 16 * 1024;
pub(crate) const HEADER_BYTES: usize = 52;
pub(crate) const BODY_BYTES: usize = FRAME_BYTES - 32;
const SCHEMA: u16 = 1;
#[cfg(target_os = "linux")]
pub(crate) const PLATFORM: u8 = 1;
#[cfg(target_os = "macos")]
pub(crate) const PLATFORM: u8 = 2;
#[cfg(target_os = "windows")]
pub(crate) const PLATFORM: u8 = 3;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("control frames require a supported Linux, macOS, or Windows target");
#[derive(Clone, Copy)]
pub(crate) enum Domain {
    Recovery,
    Successor,
}
impl Domain {
    fn parts(&self) -> (&'static [u8; 8], &'static [u8], u64) {
        match self {
            Self::Recovery => (b"AXRECV01", b"axial.fs.recovery-frame.v1\0", 0),
            Self::Successor => (b"AXSUCC01", b"axial.fs.successor-frame.v1\0", 2_097_152),
        }
    }
}
#[derive(Debug)]
pub(crate) struct Error;
pub(crate) type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Address {
    pub(crate) lane: [u8; 16],
    pub(crate) slot: u8,
    pub(crate) side: u8,
}
impl Address {
    pub(crate) fn new(lane: [u8; 16], slot: u8, side: u8) -> Result<Self> {
        require(lane != [0; 16] && usize::from(slot) < 64 && usize::from(side) < 2)?;
        Ok(Self { lane, slot, side })
    }
}
pub(crate) struct Envelope<'a> {
    pub(crate) address: Address,
    pub(crate) generation: u64,
    pub(crate) kind: u8,
    pub(crate) payload: &'a [u8],
}
pub(crate) enum Probe<'a> {
    Empty,
    Torn,
    Valid(Envelope<'a>),
}
pub(crate) fn offset(domain: Domain, slot: u8, side: u8) -> Result<u64> {
    Address::new([1; 16], slot, side)?;
    Ok(domain.parts().2 + (usize::from(slot) * 2 + usize::from(side)) as u64 * FRAME_BYTES as u64)
}
pub(crate) fn encode(
    domain: Domain,
    address: Address,
    generation: u64,
    kind: u8,
    payload: &[u8],
) -> Result<[u8; FRAME_BYTES]> {
    Address::new(address.lane, address.slot, address.side)?;
    require(generation != 0 && address.side == generation_side(generation))?;
    require(HEADER_BYTES.checked_add(payload.len()).ok_or(Error)? <= BODY_BYTES)?;
    let mut output = [0; FRAME_BYTES];
    output[..8].copy_from_slice(domain.parts().0);
    output[8..10].copy_from_slice(&SCHEMA.to_le_bytes());
    output[10..12].copy_from_slice(&[PLATFORM, kind]);
    output[12..28].copy_from_slice(&address.lane);
    output[28..30].copy_from_slice(&[address.slot, address.side]);
    output[36..44].copy_from_slice(&generation.to_le_bytes());
    let payload_len = u32::try_from(payload.len()).map_err(|_| Error)?;
    output[44..48].copy_from_slice(&payload_len.to_le_bytes());
    output[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(payload);
    let digest = checksum(domain, &output[..BODY_BYTES], address.side);
    output[BODY_BYTES..].copy_from_slice(&digest);
    Ok(output)
}
pub(crate) fn probe(domain: Domain, bytes: &[u8], slot: u8, side: u8) -> Result<Probe<'_>> {
    require(bytes.len() == FRAME_BYTES)?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Ok(Probe::Empty);
    }
    let (body, observed) = bytes.split_at(BODY_BYTES);
    let declared_side = bytes[29];
    if observed != checksum(domain, body, declared_side) {
        return Ok(Probe::Torn);
    }
    require(&body[..8] == domain.parts().0 && body[8..10] == SCHEMA.to_le_bytes())?;
    let kind = body[11];
    let lane = body[12..28].try_into().map_err(|_| Error)?;
    let address = Address::new(lane, body[28], body[29])?;
    require(
        body[10] == PLATFORM
            && address.slot == slot
            && address.side == side
            && body[30..36].iter().all(|byte| *byte == 0),
    )?;
    let generation = u64::from_le_bytes(body[36..44].try_into().map_err(|_| Error)?);
    require(generation != 0 && side == generation_side(generation))?;
    let encoded_len = u32::from_le_bytes(body[44..48].try_into().map_err(|_| Error)?);
    let len = usize::try_from(encoded_len).map_err(|_| Error)?;
    let end = HEADER_BYTES.checked_add(len).ok_or(Error)?;
    require(body[48..HEADER_BYTES].iter().all(|byte| *byte == 0))?;
    let payload = body.get(HEADER_BYTES..end).ok_or(Error)?;
    require(body[end..].iter().all(|byte| *byte == 0))?;
    Ok(Probe::Valid(Envelope {
        address,
        generation,
        kind,
        payload,
    }))
}
pub(crate) fn generation_side(generation: u64) -> u8 {
    u8::from(generation.is_multiple_of(2))
}
pub(crate) fn checksum(domain: Domain, body: &[u8], side: u8) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain.parts().1);
    hash.update([side]);
    hash.update(body);
    hash.finalize().into()
}
fn require(valid: bool) -> Result<()> {
    valid.then_some(()).ok_or(Error)
}
pub(crate) struct Cursor<'a>(pub(crate) &'a [u8]);
impl<'a> Cursor<'a> {
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let (value, rest) = self.0.split_at_checked(len).ok_or(Error)?;
        self.0 = rest;
        Ok(value)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error)
    }
}
