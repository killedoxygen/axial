use crate::managed_component_table::{
    COMPONENT_TABLE_HEADER_BYTES, COMPONENT_TABLE_ROW_PREFIX_BYTES, COMPONENT_TABLE_ROWS_PER_SHARD,
    ComponentIntentManifest, ComponentShardDescriptor, MAX_COMPONENT_PATH_BYTES,
    MAX_COMPONENT_TABLE_ROWS, MAX_COMPONENT_TABLE_SHARDS, encode_component_intent_manifest,
    expected_shard_count,
};
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

const MAX_COMPONENT_ENCODED_ROW_BYTES: usize =
    COMPONENT_TABLE_ROW_PREFIX_BYTES + MAX_COMPONENT_PATH_BYTES + 28;
pub(crate) const MAX_COMPONENT_TABLE_SPOOL_BYTES: usize = MAX_COMPONENT_TABLE_SHARDS
    * COMPONENT_TABLE_HEADER_BYTES
    + MAX_COMPONENT_TABLE_ROWS * MAX_COMPONENT_ENCODED_ROW_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed component table spool is invalid")]
pub(crate) struct ComponentTableSpoolError;

pub(crate) struct ComponentTableSpool {
    file: File,
    descriptors: Vec<ComponentShardDescriptor>,
    total_rows: usize,
    expected_shards: usize,
    max_bytes: u64,
    total_bytes: u64,
    poisoned: bool,
}

pub(crate) struct ComponentTableReplay {
    file: File,
    descriptors: Vec<ComponentShardDescriptor>,
    total_bytes: u64,
    next_shard: usize,
    next_offset: u64,
    poisoned: bool,
}

impl ComponentTableSpool {
    pub(crate) fn new(total_rows: usize) -> Result<Self, ComponentTableSpoolError> {
        let expected_shards =
            expected_shard_count(total_rows).map_err(|_| ComponentTableSpoolError)?;
        let max_bytes = expected_shards
            .checked_mul(COMPONENT_TABLE_HEADER_BYTES)
            .and_then(|bytes| {
                total_rows
                    .checked_mul(MAX_COMPONENT_ENCODED_ROW_BYTES)
                    .and_then(|rows| bytes.checked_add(rows))
            })
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ComponentTableSpoolError)?;
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(expected_shards)
            .map_err(|_| ComponentTableSpoolError)?;
        Ok(Self {
            file: tempfile::tempfile().map_err(|_| ComponentTableSpoolError)?,
            descriptors,
            total_rows,
            expected_shards,
            max_bytes,
            total_bytes: 0,
            poisoned: false,
        })
    }

    pub(crate) fn append(
        &mut self,
        encoded: Vec<u8>,
        descriptor: ComponentShardDescriptor,
    ) -> Result<(), ComponentTableSpoolError> {
        if self.poisoned || !self.append_shape_is_valid(&encoded, &descriptor) {
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        }
        let encoded_len = u64::try_from(encoded.len()).map_err(|_| {
            self.poisoned = true;
            ComponentTableSpoolError
        })?;
        let next_total = self.total_bytes.checked_add(encoded_len).ok_or_else(|| {
            self.poisoned = true;
            ComponentTableSpoolError
        })?;
        if next_total > self.max_bytes
            || !self
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.len() == self.total_bytes)
            || !self
                .file
                .stream_position()
                .is_ok_and(|position| position == self.total_bytes)
            || self.file.write_all(&encoded).is_err()
            || !self
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.len() == next_total)
        {
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        }
        self.total_bytes = next_total;
        self.descriptors.push(descriptor);
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        manifest: &ComponentIntentManifest,
    ) -> Result<ComponentTableReplay, ComponentTableSpoolError> {
        let descriptor_bytes = self
            .descriptors
            .iter()
            .try_fold(0_u64, |total, descriptor| {
                total.checked_add(u64::from(descriptor.byte_len))
            })
            .ok_or(ComponentTableSpoolError)?;
        if self.poisoned
            || usize::try_from(manifest.total_rows) != Ok(self.total_rows)
            || self.descriptors != manifest.shards
            || encode_component_intent_manifest(manifest).is_err()
            || descriptor_bytes != self.total_bytes
            || self.file.flush().is_err()
            || !self
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.len() == self.total_bytes)
            || !self
                .file
                .seek(SeekFrom::Start(0))
                .is_ok_and(|position| position == 0)
        {
            return Err(ComponentTableSpoolError);
        }
        Ok(ComponentTableReplay {
            file: self.file,
            descriptors: self.descriptors,
            total_bytes: self.total_bytes,
            next_shard: 0,
            next_offset: 0,
            poisoned: false,
        })
    }

    fn append_shape_is_valid(&self, encoded: &[u8], descriptor: &ComponentShardDescriptor) -> bool {
        let index = self.descriptors.len();
        if index >= self.expected_shards {
            return false;
        }
        let Some(first_row) = index.checked_mul(COMPONENT_TABLE_ROWS_PER_SHARD) else {
            return false;
        };
        let Some(expected_rows) = self
            .total_rows
            .checked_sub(first_row)
            .map(|rows| rows.min(COMPONENT_TABLE_ROWS_PER_SHARD))
        else {
            return false;
        };
        let Some(max_encoded_len) = expected_rows
            .checked_mul(MAX_COMPONENT_ENCODED_ROW_BYTES)
            .and_then(|rows| COMPONENT_TABLE_HEADER_BYTES.checked_add(rows))
        else {
            return false;
        };
        let Ok(index) = u32::try_from(index) else {
            return false;
        };
        let Ok(first_row) = u32::try_from(first_row) else {
            return false;
        };
        let Ok(encoded_len) = u32::try_from(encoded.len()) else {
            return false;
        };
        descriptor.shard_index == index
            && descriptor.first_row == first_row
            && descriptor.row_count as usize == expected_rows
            && descriptor.byte_len == encoded_len
            && encoded.len() >= COMPONENT_TABLE_HEADER_BYTES
            && encoded.len() <= max_encoded_len
            && descriptor.sha256 == <[u8; 32]>::from(Sha256::digest(encoded))
    }
}

impl ComponentTableReplay {
    pub(crate) fn next(
        &mut self,
    ) -> Result<Option<(ComponentShardDescriptor, Vec<u8>)>, ComponentTableSpoolError> {
        if self.poisoned {
            return Err(ComponentTableSpoolError);
        }
        let Some(descriptor) = self.descriptors.get(self.next_shard).cloned() else {
            if self.next_offset == self.total_bytes
                && self
                    .file
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() == self.total_bytes)
                && self
                    .file
                    .stream_position()
                    .is_ok_and(|position| position == self.total_bytes)
            {
                return Ok(None);
            }
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        };
        if !self
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.len() == self.total_bytes)
            || !self
                .file
                .stream_position()
                .is_ok_and(|position| position == self.next_offset)
        {
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        }
        let length = usize::try_from(descriptor.byte_len).map_err(|_| {
            self.poisoned = true;
            ComponentTableSpoolError
        })?;
        let mut encoded = Vec::new();
        if encoded.try_reserve_exact(length).is_err() {
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        }
        encoded.resize(length, 0);
        if self.file.read_exact(&mut encoded).is_err()
            || descriptor.sha256 != <[u8; 32]>::from(Sha256::digest(&encoded))
        {
            self.poisoned = true;
            return Err(ComponentTableSpoolError);
        }
        self.next_offset = self
            .next_offset
            .checked_add(u64::from(descriptor.byte_len))
            .ok_or_else(|| {
                self.poisoned = true;
                ComponentTableSpoolError
            })?;
        self.next_shard += 1;
        Ok(Some((descriptor, encoded)))
    }

    #[cfg(test)]
    fn overwrite_for_test(&mut self, offset: u64, byte: u8) {
        self.file
            .seek(SeekFrom::Start(offset))
            .expect("seek spool byte");
        self.file.write_all(&[byte]).expect("overwrite spool byte");
        self.file
            .seek(SeekFrom::Start(self.next_offset))
            .expect("restore spool cursor");
    }

    #[cfg(test)]
    fn truncate_for_test(&self, length: u64) {
        self.file.set_len(length).expect("truncate spool");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(index: usize, row_count: usize, bytes: &[u8]) -> ComponentShardDescriptor {
        ComponentShardDescriptor {
            shard_index: u32::try_from(index).unwrap(),
            first_row: u32::try_from(index * COMPONENT_TABLE_ROWS_PER_SHARD).unwrap(),
            row_count: u32::try_from(row_count).unwrap(),
            byte_len: u32::try_from(bytes.len()).unwrap(),
            final_bytes: row_count as u64,
            prior_bytes: 0,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    fn manifest(descriptors: Vec<ComponentShardDescriptor>) -> ComponentIntentManifest {
        ComponentIntentManifest {
            component: crate::managed_component_table::ManagedComponentKind::Assets,
            total_rows: descriptors
                .iter()
                .map(|descriptor| descriptor.row_count)
                .sum(),
            final_bytes: descriptors
                .iter()
                .map(|descriptor| descriptor.final_bytes)
                .sum(),
            prior_bytes: 0,
            transaction_nonce: [0x11; 16],
            root_binding_sha256: [0x22; 32],
            logical_rows_sha256: [0x33; 32],
            projection_sha256: [0x44; 32],
            shards: descriptors,
        }
    }

    fn shard_bytes(byte: u8, length: usize) -> Vec<u8> {
        vec![byte; length]
    }

    #[test]
    fn adjacent_shards_replay_in_descriptor_order_one_at_a_time() {
        let first = shard_bytes(0x11, COMPONENT_TABLE_HEADER_BYTES);
        let second = shard_bytes(0x22, COMPONENT_TABLE_HEADER_BYTES + 1);
        let first_descriptor = descriptor(0, COMPONENT_TABLE_ROWS_PER_SHARD, &first);
        let second_descriptor = descriptor(1, 1, &second);
        let intent = manifest(vec![first_descriptor.clone(), second_descriptor.clone()]);
        let mut spool = ComponentTableSpool::new(257).unwrap();
        spool
            .append(first.clone(), first_descriptor.clone())
            .unwrap();
        spool
            .append(second.clone(), second_descriptor.clone())
            .unwrap();

        let mut replay = spool.finish(&intent).unwrap();
        assert_eq!(replay.next().unwrap(), Some((first_descriptor, first)));
        assert_eq!(replay.next().unwrap(), Some((second_descriptor, second)));
        assert_eq!(replay.next().unwrap(), None);
        assert_eq!(replay.next().unwrap(), None);
    }

    #[test]
    fn append_rejects_wrong_geometry_hash_size_and_after_partial_shard() {
        let bytes = shard_bytes(0x11, COMPONENT_TABLE_HEADER_BYTES);
        let mut wrong_index = descriptor(1, 1, &bytes);
        let mut spool = ComponentTableSpool::new(1).unwrap();
        assert_eq!(
            spool.append(bytes.clone(), wrong_index.clone()),
            Err(ComponentTableSpoolError)
        );
        assert_eq!(
            spool.append(bytes.clone(), descriptor(0, 1, &bytes)),
            Err(ComponentTableSpoolError)
        );

        let mut spool = ComponentTableSpool::new(1).unwrap();
        wrong_index = descriptor(0, 1, &bytes);
        wrong_index.sha256[0] ^= 1;
        assert_eq!(
            spool.append(bytes.clone(), wrong_index),
            Err(ComponentTableSpoolError)
        );

        let oversized = shard_bytes(
            0x22,
            COMPONENT_TABLE_HEADER_BYTES + MAX_COMPONENT_ENCODED_ROW_BYTES + 1,
        );
        let mut spool = ComponentTableSpool::new(1).unwrap();
        assert_eq!(
            spool.append(oversized.clone(), descriptor(0, 1, &oversized)),
            Err(ComponentTableSpoolError)
        );

        let mut spool = ComponentTableSpool::new(257).unwrap();
        spool
            .append(
                bytes.clone(),
                descriptor(0, COMPONENT_TABLE_ROWS_PER_SHARD, &bytes),
            )
            .unwrap();
        assert_eq!(
            spool.append(bytes.clone(), descriptor(1, 2, &bytes)),
            Err(ComponentTableSpoolError)
        );
    }

    #[test]
    fn finish_binds_the_exact_manifest_descriptors() {
        let bytes = shard_bytes(0x11, COMPONENT_TABLE_HEADER_BYTES);
        let descriptor = descriptor(0, 1, &bytes);
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(bytes, descriptor.clone()).unwrap();
        let mut drifted = descriptor.clone();
        drifted.final_bytes += 1;
        assert!(spool.finish(&manifest(vec![drifted])).is_err());

        let empty = ComponentTableSpool::new(0).unwrap();
        let mut replay = empty.finish(&manifest(Vec::new())).unwrap();
        assert_eq!(replay.next().unwrap(), None);
    }

    #[test]
    fn replay_truncation_and_corruption_poison_all_later_reads() {
        let bytes = shard_bytes(0x11, COMPONENT_TABLE_HEADER_BYTES);
        let descriptor = descriptor(0, 1, &bytes);
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(bytes.clone(), descriptor.clone()).unwrap();
        let mut replay = spool.finish(&manifest(vec![descriptor.clone()])).unwrap();
        replay.overwrite_for_test(0, 0x99);
        assert_eq!(replay.next(), Err(ComponentTableSpoolError));
        assert_eq!(replay.next(), Err(ComponentTableSpoolError));

        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(bytes, descriptor.clone()).unwrap();
        let mut replay = spool.finish(&manifest(vec![descriptor])).unwrap();
        replay.truncate_for_test((COMPONENT_TABLE_HEADER_BYTES - 1) as u64);
        assert_eq!(replay.next(), Err(ComponentTableSpoolError));
        assert_eq!(replay.next(), Err(ComponentTableSpoolError));
    }

    #[test]
    fn declared_bounds_are_exact_without_allocating_the_aggregate_maximum() {
        assert_eq!(MAX_COMPONENT_TABLE_SPOOL_BYTES, 116_881_328);
        let bytes = shard_bytes(
            0x55,
            COMPONENT_TABLE_HEADER_BYTES + MAX_COMPONENT_ENCODED_ROW_BYTES,
        );
        let descriptor = descriptor(0, 1, &bytes);
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(bytes.clone(), descriptor.clone()).unwrap();
        let mut replay = spool.finish(&manifest(vec![descriptor.clone()])).unwrap();
        assert_eq!(replay.next().unwrap(), Some((descriptor, bytes)));
        assert_eq!(replay.next().unwrap(), None);
    }
}
