use sha1::{Digest as _, Sha1};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetainedComponentSourceSpoolError(RetainedComponentSourceSpoolErrorKind);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedComponentSourceSpoolErrorKind {
    CapacityExceeded,
    OperationFailed,
}

impl RetainedComponentSourceSpoolError {
    fn capacity_exceeded() -> Self {
        Self(RetainedComponentSourceSpoolErrorKind::CapacityExceeded)
    }

    fn operation_failed() -> Self {
        Self(RetainedComponentSourceSpoolErrorKind::OperationFailed)
    }

    pub(crate) fn is_capacity_exceeded(self) -> bool {
        self.0 == RetainedComponentSourceSpoolErrorKind::CapacityExceeded
    }
}

impl std::fmt::Display for RetainedComponentSourceSpoolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            RetainedComponentSourceSpoolErrorKind::CapacityExceeded => {
                formatter.write_str("retained component source capacity is exhausted")
            }
            RetainedComponentSourceSpoolErrorKind::OperationFailed => {
                formatter.write_str("retained component source spool operation failed")
            }
        }
    }
}

impl std::error::Error for RetainedComponentSourceSpoolError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetainedComponentSourceAppendError {
    SourceRejected,
    Spool(RetainedComponentSourceSpoolError),
}

impl std::fmt::Display for RetainedComponentSourceAppendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceRejected => {
                formatter.write_str("retained component source authentication failed")
            }
            Self::Spool(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RetainedComponentSourceAppendError {}

impl From<RetainedComponentSourceSpoolError> for RetainedComponentSourceAppendError {
    fn from(error: RetainedComponentSourceSpoolError) -> Self {
        Self::Spool(error)
    }
}

struct RetainedComponentSourceBudget {
    limit_bytes: u64,
    remaining_bytes: Mutex<u64>,
}

pub(crate) struct RetainedComponentSourceSpool {
    budget: RetainedComponentSourceBudget,
    state: Mutex<RetainedComponentSourceSpoolState>,
}

struct RetainedComponentSourceSpoolState {
    file: File,
    high_water: u64,
    valid: bool,
}

pub(crate) struct RetainedComponentSourceAllocation {
    spool: Arc<RetainedComponentSourceSpool>,
    offset: u64,
    length: u64,
}

pub(crate) struct RetainedComponentSourceReader {
    spool: Arc<RetainedComponentSourceSpool>,
    offset: u64,
    length: u64,
    position: u64,
}

impl RetainedComponentSourceBudget {
    fn new(bytes: u64) -> Self {
        Self {
            limit_bytes: bytes,
            remaining_bytes: Mutex::new(bytes),
        }
    }

    fn try_reserve(&self, bytes: u64) -> Result<(), RetainedComponentSourceSpoolError> {
        let mut available = self
            .remaining_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available = available
            .checked_sub(bytes)
            .ok_or_else(RetainedComponentSourceSpoolError::capacity_exceeded)?;
        Ok(())
    }

    fn refund(&self, bytes: u64) -> Result<(), RetainedComponentSourceSpoolError> {
        let mut available = self
            .remaining_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let refunded = available
            .checked_add(bytes)
            .ok_or_else(RetainedComponentSourceSpoolError::operation_failed)?;
        if refunded > self.limit_bytes {
            return Err(RetainedComponentSourceSpoolError::operation_failed());
        }
        *available = refunded;
        Ok(())
    }

    #[cfg(test)]
    fn available_bytes(&self) -> u64 {
        *self
            .remaining_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RetainedComponentSourceSpoolState {
    fn validate_integrity(&mut self) -> Result<(), RetainedComponentSourceSpoolError> {
        if !self.valid {
            return Err(RetainedComponentSourceSpoolError::operation_failed());
        }
        let physical_length = match self.file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => {
                self.valid = false;
                return Err(RetainedComponentSourceSpoolError::operation_failed());
            }
        };
        if physical_length != self.high_water {
            self.valid = false;
            return Err(RetainedComponentSourceSpoolError::operation_failed());
        }
        Ok(())
    }

    fn poison(&mut self) {
        self.valid = false;
    }
}

impl RetainedComponentSourceSpool {
    pub(crate) fn new(bytes: u64) -> Result<Arc<Self>, RetainedComponentSourceSpoolError> {
        Ok(Arc::new(Self {
            budget: RetainedComponentSourceBudget::new(bytes),
            state: Mutex::new(RetainedComponentSourceSpoolState {
                file: tempfile::tempfile()
                    .map_err(|_| RetainedComponentSourceSpoolError::operation_failed())?,
                high_water: 0,
                valid: true,
            }),
        }))
    }

    pub(crate) fn append_authenticated<R>(
        self: &Arc<Self>,
        mut source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<RetainedComponentSourceAllocation, RetainedComponentSourceAppendError>
    where
        R: Read + Seek,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.validate_integrity()?;
        self.budget.try_reserve(expected_size)?;
        let offset = state.high_water;
        let Some(end) = offset.checked_add(expected_size) else {
            self.budget.refund(expected_size)?;
            return Err(RetainedComponentSourceSpoolError::operation_failed().into());
        };
        if source.seek(SeekFrom::Start(0)).is_err() {
            self.rollback_provisional(&mut state, offset, expected_size)?;
            return Err(RetainedComponentSourceAppendError::SourceRejected);
        }
        if state.file.seek(SeekFrom::Start(offset)).is_err() {
            state.poison();
            self.budget.refund(expected_size)?;
            return Err(RetainedComponentSourceSpoolError::operation_failed().into());
        }

        let mut observed = 0_u64;
        let mut hasher = Sha1::new();
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = match source.read(&mut chunk) {
                Ok(read) => read,
                Err(_) => {
                    self.rollback_provisional(&mut state, offset, expected_size)?;
                    return Err(RetainedComponentSourceAppendError::SourceRejected);
                }
            };
            if read == 0 {
                break;
            }
            let Some(next) = observed.checked_add(read as u64) else {
                self.rollback_provisional(&mut state, offset, expected_size)?;
                return Err(RetainedComponentSourceAppendError::SourceRejected);
            };
            if next > expected_size {
                self.rollback_provisional(&mut state, offset, expected_size)?;
                return Err(RetainedComponentSourceAppendError::SourceRejected);
            }
            if state.file.write_all(&chunk[..read]).is_err() {
                self.rollback_provisional(&mut state, offset, expected_size)?;
                return Err(RetainedComponentSourceSpoolError::operation_failed().into());
            }
            observed = next;
            hasher.update(&chunk[..read]);
        }
        if observed != expected_size || <[u8; 20]>::from(hasher.finalize()) != expected_sha1 {
            self.rollback_provisional(&mut state, offset, expected_size)?;
            return Err(RetainedComponentSourceAppendError::SourceRejected);
        }
        if state.file.flush().is_err() {
            self.rollback_provisional(&mut state, offset, expected_size)?;
            return Err(RetainedComponentSourceSpoolError::operation_failed().into());
        }
        match state.file.metadata() {
            Ok(metadata) if metadata.len() == end => {}
            Ok(_) | Err(_) => {
                self.rollback_provisional(&mut state, offset, expected_size)?;
                return Err(RetainedComponentSourceSpoolError::operation_failed().into());
            }
        }
        state.high_water = end;
        Ok(RetainedComponentSourceAllocation {
            spool: Arc::clone(self),
            offset,
            length: expected_size,
        })
    }

    fn rollback_provisional(
        &self,
        state: &mut RetainedComponentSourceSpoolState,
        offset: u64,
        reserved_bytes: u64,
    ) -> Result<(), RetainedComponentSourceSpoolError> {
        let rollback = (|| -> io::Result<()> {
            state.file.set_len(offset)?;
            state.file.seek(SeekFrom::Start(offset))?;
            state.file.flush()?;
            if state.file.metadata()?.len() != offset || state.high_water != offset {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "retained component source spool rollback could not be proven",
                ));
            }
            Ok(())
        })();
        if rollback.is_err() {
            state.poison();
            return Err(RetainedComponentSourceSpoolError::operation_failed());
        }
        self.budget.refund(reserved_bytes)
    }

    #[cfg(test)]
    pub(crate) fn available_bytes(&self) -> u64 {
        self.budget.available_bytes()
    }
}

impl RetainedComponentSourceAllocation {
    pub(crate) fn into_reader(
        self,
    ) -> Result<RetainedComponentSourceReader, RetainedComponentSourceSpoolError> {
        self.reader()
    }

    pub(crate) fn replay_reader(
        &self,
    ) -> Result<RetainedComponentSourceReader, RetainedComponentSourceSpoolError> {
        self.reader()
    }

    fn reader(&self) -> Result<RetainedComponentSourceReader, RetainedComponentSourceSpoolError> {
        let mut state = self
            .spool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let end = self
            .offset
            .checked_add(self.length)
            .ok_or_else(RetainedComponentSourceSpoolError::operation_failed)?;
        state.validate_integrity()?;
        if end > state.high_water {
            state.poison();
            return Err(RetainedComponentSourceSpoolError::operation_failed());
        }
        drop(state);
        Ok(RetainedComponentSourceReader {
            spool: Arc::clone(&self.spool),
            offset: self.offset,
            length: self.length,
            position: 0,
        })
    }
}

impl Read for RetainedComponentSourceReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        if remaining == 0 || output.is_empty() {
            return Ok(0);
        }
        let read_bound = usize::try_from(remaining.min(output.len() as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained component source read overflow",
            )
        })?;
        let mut state = self
            .spool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.validate_integrity().map_err(io::Error::other)?;
        let offset = self.offset.checked_add(self.position).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained component source offset overflow",
            )
        })?;
        if let Err(error) = state.file.seek(SeekFrom::Start(offset)) {
            state.poison();
            return Err(error);
        }
        let read = match state.file.read(&mut output[..read_bound]) {
            Ok(read) => read,
            Err(error) => {
                state.poison();
                return Err(error);
            }
        };
        if read == 0 {
            state.poison();
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "retained component source spool ended inside an admitted allocation",
            ));
        }
        self.position = self.position.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained component source position overflow",
            )
        })?;
        Ok(read)
    }
}

impl Seek for RetainedComponentSourceReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
        };
        if !(0..=i128::from(self.length)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained component source seek escaped its allocation",
            ));
        }
        self.position = u64::try_from(next).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained component source seek overflow",
            )
        })?;
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sha1(bytes: &[u8]) -> [u8; 20] {
        Sha1::digest(bytes).into()
    }

    fn append(
        spool: &Arc<RetainedComponentSourceSpool>,
        bytes: &[u8],
    ) -> RetainedComponentSourceAllocation {
        spool
            .append_authenticated(Cursor::new(bytes), bytes.len() as u64, sha1(bytes))
            .expect("append authenticated component source")
    }

    fn read_all(allocation: &RetainedComponentSourceAllocation) -> Vec<u8> {
        let mut reader = allocation.replay_reader().expect("replay allocation");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read allocation");
        bytes
    }

    #[test]
    fn admits_arbitrary_and_zero_length_authenticated_sources() {
        let bytes = b"component bytes that are not a JAR";
        let spool =
            RetainedComponentSourceSpool::new(bytes.len() as u64).expect("component source spool");

        let arbitrary = append(&spool, bytes);
        let empty = append(&spool, b"");

        assert_eq!(read_all(&arbitrary), bytes);
        assert!(read_all(&empty).is_empty());
        assert_eq!(spool.available_bytes(), 0);

        let zero_spool =
            RetainedComponentSourceSpool::new(0).expect("zero-capacity component source spool");
        assert!(read_all(&append(&zero_spool, b"")).is_empty());
        assert_eq!(zero_spool.available_bytes(), 0);
    }

    #[test]
    fn rejected_sources_rollback_bytes_budget_and_tail_position() {
        let first = b"first arbitrary component source";
        let tail = b"tail";
        let spool = RetainedComponentSourceSpool::new((first.len() + tail.len()) as u64)
            .expect("component source spool");
        let first_allocation = append(&spool, first);
        let available = spool.available_bytes();

        assert!(matches!(
            spool.append_authenticated(Cursor::new(tail), tail.len() as u64, [0; 20]),
            Err(RetainedComponentSourceAppendError::SourceRejected)
        ));
        assert_eq!(spool.available_bytes(), available);
        assert!(matches!(
            spool.append_authenticated(Cursor::new(b"x"), tail.len() as u64, sha1(b"x"),),
            Err(RetainedComponentSourceAppendError::SourceRejected)
        ));
        assert_eq!(spool.available_bytes(), available);
        assert!(matches!(
            spool.append_authenticated(
                Cursor::new(tail),
                tail.len() as u64 - 1,
                sha1(&tail[..tail.len() - 1]),
            ),
            Err(RetainedComponentSourceAppendError::SourceRejected)
        ));
        assert_eq!(spool.available_bytes(), available);

        let tail_allocation = append(&spool, tail);
        assert_eq!(spool.available_bytes(), 0);
        assert_eq!(read_all(&first_allocation), first);
        assert_eq!(read_all(&tail_allocation), tail);
    }

    #[test]
    fn adjacent_allocations_replay_and_seek_within_unequal_bounds() {
        let first = b"abc";
        let second = b"unequal adjacent component bytes";
        let spool = RetainedComponentSourceSpool::new((first.len() + second.len()) as u64)
            .expect("component source spool");
        let first_allocation = append(&spool, first);
        let second_allocation = append(&spool, second);

        let mut first_reader = first_allocation.into_reader().expect("first reader");
        assert!(
            first_reader
                .seek(SeekFrom::Start(first.len() as u64 + 1))
                .is_err()
        );
        assert!(first_reader.seek(SeekFrom::End(1)).is_err());
        let mut first_bytes = Vec::new();
        first_reader
            .read_to_end(&mut first_bytes)
            .expect("read first allocation");
        assert_eq!(first_bytes, first);

        let mut second_reader = second_allocation.into_reader().expect("second reader");
        second_reader
            .seek(SeekFrom::End(-5))
            .expect("seek second allocation");
        let mut suffix = Vec::new();
        second_reader
            .read_to_end(&mut suffix)
            .expect("read second allocation suffix");
        assert_eq!(suffix, &second[second.len() - 5..]);
    }

    #[test]
    fn physical_length_corruption_poisons_all_allocations_and_later_appends() {
        let first = b"first retained component allocation";
        let second = b"second retained component allocation";
        let later = b"later";
        let spool =
            RetainedComponentSourceSpool::new((first.len() + second.len() + later.len()) as u64)
                .expect("component source spool");
        let first_allocation = append(&spool, first);
        let second_allocation = append(&spool, second);
        let mut first_reader = first_allocation.into_reader().expect("first reader");
        let remaining_before_corruption = spool.available_bytes();

        {
            let state = spool
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .file
                .set_len(state.high_water + 1)
                .expect("corrupt spool length");
        }

        assert!(first_reader.read(&mut [0]).is_err());
        assert!(second_allocation.into_reader().is_err());
        assert!(matches!(
            spool.append_authenticated(Cursor::new(later), later.len() as u64, sha1(later)),
            Err(RetainedComponentSourceAppendError::Spool(_))
        ));
        assert_eq!(spool.available_bytes(), remaining_before_corruption);
    }
}
