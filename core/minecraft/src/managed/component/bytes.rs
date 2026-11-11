//! Little-endian field encoding shared by the component publication records.

use crate::managed::component::table::ComponentTableError;

pub(crate) fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub(crate) struct ByteCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ByteCursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn total_len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn finished(&self) -> bool {
        self.position == self.bytes.len()
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], ComponentTableError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(ComponentTableError)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    pub(crate) fn expect(&mut self, expected: &[u8]) -> Result<(), ComponentTableError> {
        if self.take(expected.len())? != expected {
            return Err(ComponentTableError);
        }
        Ok(())
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], ComponentTableError> {
        self.take(N)?.try_into().map_err(|_| ComponentTableError)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ComponentTableError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, ComponentTableError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ComponentTableError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ComponentTableError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}
