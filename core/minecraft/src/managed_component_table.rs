use crate::portable_path::{
    PortablePathKey, PortableRelativePath, MAX_PORTABLE_RELATIVE_PATH_BYTES,
};
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, MAX_TIER2_ENTRIES};
use crate::managed_fs::MAX_MANAGED_DIRECTORY_ENTRIES;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;

pub(crate) const COMPONENT_TABLE_ROWS_PER_SHARD: usize = 256;
pub(crate) const MAX_COMPONENT_TABLE_ROWS: usize = MAX_TIER2_ENTRIES;
pub(crate) const MAX_COMPONENT_TABLE_SHARDS: usize =
    MAX_COMPONENT_TABLE_ROWS.div_ceil(COMPONENT_TABLE_ROWS_PER_SHARD);
pub(crate) const MAX_COMPONENT_PATH_BYTES: usize = MAX_PORTABLE_RELATIVE_PATH_BYTES;
pub(crate) const MAX_CREATED_ANCESTORS: usize = 800_000;
pub(crate) const MAX_CREATED_ANCESTOR_PATH_BYTES: usize = 256 << 20;
pub(crate) const COMPONENT_TABLE_HEADER_BYTES: usize = 104;
pub(crate) const COMPONENT_TABLE_ROW_PREFIX_BYTES: usize = 44;
pub(crate) const MAX_COMPONENT_TABLE_SHARD_BYTES: usize = COMPONENT_TABLE_HEADER_BYTES
    + COMPONENT_TABLE_ROWS_PER_SHARD
        * (COMPONENT_TABLE_ROW_PREFIX_BYTES + MAX_COMPONENT_PATH_BYTES + 28);
pub(crate) const COMPONENT_INTENT_HEADER_BYTES: usize = 160;
pub(crate) const COMPONENT_INTENT_DESCRIPTOR_BYTES: usize = 64;
pub(crate) const MAX_COMPONENT_INTENT_BYTES: usize =
    COMPONENT_INTENT_HEADER_BYTES + MAX_COMPONENT_TABLE_SHARDS * COMPONENT_INTENT_DESCRIPTOR_BYTES;

const TABLE_MAGIC: &[u8; 8] = b"AXCPTBL\0";
const INTENT_MAGIC: &[u8; 8] = b"AXCPINT\0";
const FORMAT_VERSION: u16 = 1;
const PRIOR_PRESENT: u8 = 1;
const NO_CREATED_ANCESTOR: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum ManagedComponentKind {
    Libraries = 1,
    Assets = 2,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum ManagedComponentArtifactKind {
    Library = 1,
    NativeLibrary = 2,
    AssetIndex = 3,
    AssetObject = 4,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentPriorFile {
    pub(crate) size: u64,
    pub(crate) sha1: [u8; 20],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentTableRow {
    pub(crate) inventory_ordinal: u32,
    pub(crate) final_size: u64,
    pub(crate) final_sha1: [u8; 20],
    pub(crate) kind: ManagedComponentArtifactKind,
    pub(crate) path: PortableRelativePath,
    pub(crate) first_created_depth: Option<u16>,
    pub(crate) prior: Option<ComponentPriorFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentTableShard {
    pub(crate) component: ManagedComponentKind,
    pub(crate) shard_index: u32,
    pub(crate) shard_count: u32,
    pub(crate) first_row: u32,
    pub(crate) total_rows: u32,
    pub(crate) transaction_nonce: [u8; 16],
    pub(crate) root_binding_sha256: [u8; 32],
    pub(crate) rows: Vec<ComponentTableRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentShardDescriptor {
    pub(crate) shard_index: u32,
    pub(crate) first_row: u32,
    pub(crate) row_count: u32,
    pub(crate) byte_len: u32,
    pub(crate) final_bytes: u64,
    pub(crate) prior_bytes: u64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentIntentManifest {
    pub(crate) component: ManagedComponentKind,
    pub(crate) total_rows: u32,
    pub(crate) final_bytes: u64,
    pub(crate) prior_bytes: u64,
    pub(crate) transaction_nonce: [u8; 16],
    pub(crate) root_binding_sha256: [u8; 32],
    pub(crate) logical_rows_sha256: [u8; 32],
    pub(crate) projection_sha256: [u8; 32],
    pub(crate) shards: Vec<ComponentShardDescriptor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed component publication table is invalid")]
pub(crate) struct ComponentTableError;

impl ManagedComponentKind {
    pub(crate) fn from_byte(value: u8) -> Result<Self, ComponentTableError> {
        match value {
            1 => Ok(Self::Libraries),
            2 => Ok(Self::Assets),
            _ => Err(ComponentTableError),
        }
    }

    fn accepts(self, kind: ManagedComponentArtifactKind) -> bool {
        matches!(
            (self, kind),
            (
                Self::Libraries,
                ManagedComponentArtifactKind::Library | ManagedComponentArtifactKind::NativeLibrary,
            ) | (
                Self::Assets,
                ManagedComponentArtifactKind::AssetIndex
                    | ManagedComponentArtifactKind::AssetObject,
            )
        )
    }
}

impl ManagedComponentArtifactKind {
    fn from_byte(value: u8) -> Result<Self, ComponentTableError> {
        match value {
            1 => Ok(Self::Library),
            2 => Ok(Self::NativeLibrary),
            3 => Ok(Self::AssetIndex),
            4 => Ok(Self::AssetObject),
            _ => Err(ComponentTableError),
        }
    }
}

impl ComponentTableRow {
    pub(crate) fn prior_is_final(&self) -> bool {
        self.prior
            .as_ref()
            .is_some_and(|prior| prior.size == self.final_size && prior.sha1 == self.final_sha1)
    }

    fn staged_bytes(&self) -> u64 {
        if self.prior_is_final() {
            0
        } else {
            self.final_size
        }
    }

    fn quarantine_bytes(&self) -> u64 {
        match &self.prior {
            Some(prior) if !self.prior_is_final() => prior.size,
            _ => 0,
        }
    }
}

pub(crate) fn component_table_path(shard_index: usize) -> Result<String, ComponentTableError> {
    if shard_index >= MAX_COMPONENT_TABLE_SHARDS {
        return Err(ComponentTableError);
    }
    Ok(format!("table/{shard_index:06}.tbl"))
}

#[cfg(test)]
pub(crate) fn component_entry_slot(
    shard_index: usize,
    row_in_shard: usize,
) -> Result<String, ComponentTableError> {
    if shard_index >= MAX_COMPONENT_TABLE_SHARDS || row_in_shard >= COMPONENT_TABLE_ROWS_PER_SHARD {
        return Err(ComponentTableError);
    }
    Ok(format!("{shard_index:06}/{row_in_shard:03}"))
}

pub(crate) fn expected_shard_count(total_rows: usize) -> Result<usize, ComponentTableError> {
    if total_rows > MAX_COMPONENT_TABLE_ROWS {
        return Err(ComponentTableError);
    }
    total_rows
        .checked_add(COMPONENT_TABLE_ROWS_PER_SHARD - 1)
        .map(|rows| rows / COMPONENT_TABLE_ROWS_PER_SHARD)
        .filter(|count| *count <= MAX_COMPONENT_TABLE_SHARDS)
        .ok_or(ComponentTableError)
}

fn expected_shard_row_count(
    total_rows: usize,
    shard_index: usize,
) -> Result<usize, ComponentTableError> {
    let first_row = shard_index
        .checked_mul(COMPONENT_TABLE_ROWS_PER_SHARD)
        .ok_or(ComponentTableError)?;
    let remaining = total_rows
        .checked_sub(first_row)
        .ok_or(ComponentTableError)?;
    Ok(remaining.min(COMPONENT_TABLE_ROWS_PER_SHARD))
}

const INVENTORY_ORDINAL_WORDS: usize = MAX_COMPONENT_TABLE_ROWS.div_ceil(u64::BITS as usize);
const MAX_COMPONENT_PATH_STORAGE_BYTES: usize = MAX_COMPONENT_TABLE_ROWS * MAX_COMPONENT_PATH_BYTES;
const MAX_CREATED_ANCESTOR_ALLOCATION_BYTES: usize = MAX_CREATED_ANCESTOR_PATH_BYTES * 2;
const MAX_PROJECTED_PATH_PREFIXES: usize = MAX_CREATED_ANCESTORS + MAX_COMPONENT_TABLE_ROWS;
const MAX_PROJECTED_PREFIX_PATH_BYTES: usize = MAX_CREATED_ANCESTOR_PATH_BYTES;
const MAX_PROJECTED_PREFIX_ALLOCATION_BYTES: usize = MAX_PROJECTED_PREFIX_PATH_BYTES * 2;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ComponentCreatedAncestor {
    ComponentRoot,
    Relative(PortableRelativePath),
}

struct ProjectedPathPrefix {
    exact: String,
    is_file: bool,
}

struct ComponentRowValidator {
    row_count: usize,
    final_bytes: u64,
    staged_bytes: u64,
    prior_bytes: u64,
    last_path: Option<String>,
    inventory_ordinals: Vec<u64>,
    projected_prefixes: HashMap<PortablePathKey, ProjectedPathPrefix>,
    projected_prefix_path_bytes: usize,
    projected_prefix_allocation_bytes: usize,
    created_depth_checks: Vec<(String, Option<u16>)>,
    created_depth_path_bytes: usize,
    created_ancestors: HashMap<Option<PortablePathKey>, ComponentCreatedAncestor>,
    created_ancestor_path_bytes: usize,
    created_ancestor_allocation_bytes: usize,
    fanout_counts: Vec<usize>,
}

impl ComponentRowValidator {
    fn new(expected_rows: usize) -> Result<Self, ComponentTableError> {
        if expected_rows > MAX_COMPONENT_TABLE_ROWS {
            return Err(ComponentTableError);
        }
        let mut inventory_ordinals = Vec::new();
        inventory_ordinals
            .try_reserve_exact(INVENTORY_ORDINAL_WORDS)
            .map_err(|_| ComponentTableError)?;
        inventory_ordinals.resize(INVENTORY_ORDINAL_WORDS, 0);
        let mut projected_prefixes = HashMap::new();
        projected_prefixes
            .try_reserve(expected_rows)
            .map_err(|_| ComponentTableError)?;
        let mut created_depth_checks = Vec::new();
        created_depth_checks
            .try_reserve_exact(expected_rows)
            .map_err(|_| ComponentTableError)?;
        let mut fanout_counts = Vec::new();
        fanout_counts
            .try_reserve_exact(MAX_COMPONENT_PATH_BYTES.div_ceil(2))
            .map_err(|_| ComponentTableError)?;
        fanout_counts.push(0);
        Ok(Self {
            row_count: 0,
            final_bytes: 0,
            staged_bytes: 0,
            prior_bytes: 0,
            last_path: None,
            inventory_ordinals,
            projected_prefixes,
            projected_prefix_path_bytes: 0,
            projected_prefix_allocation_bytes: 0,
            created_depth_checks,
            created_depth_path_bytes: 0,
            created_ancestors: HashMap::new(),
            created_ancestor_path_bytes: 0,
            created_ancestor_allocation_bytes: 0,
            fanout_counts,
        })
    }

    fn push(
        &mut self,
        component: ManagedComponentKind,
        row: &ComponentTableRow,
    ) -> Result<(), ComponentTableError> {
        if self.row_count >= MAX_COMPONENT_TABLE_ROWS
            || usize::try_from(row.inventory_ordinal).map_err(|_| ComponentTableError)?
                >= MAX_TIER2_ENTRIES
            || row.final_size > MAX_TIER2_ARTIFACT_BYTES
            || row
                .prior
                .as_ref()
                .is_some_and(|prior| prior.size > MAX_TIER2_ARTIFACT_BYTES)
            || !component.accepts(row.kind)
            || row.path.as_str().len() > MAX_COMPONENT_PATH_BYTES
        {
            return Err(ComponentTableError);
        }
        let portable_path = row.path.key();
        self.created_depth_path_bytes = self
            .created_depth_path_bytes
            .checked_add(portable_path.as_str().len())
            .filter(|bytes| *bytes <= MAX_COMPONENT_PATH_STORAGE_BYTES)
            .ok_or(ComponentTableError)?;
        let segment_count = row.path.as_str().split('/').count();
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(segment_count)
            .map_err(|_| ComponentTableError)?;
        segments.extend(row.path.as_str().split('/'));
        if let Some(last_path) = &self.last_path
            && row.path.as_str() <= last_path.as_str()
        {
            return Err(ComponentTableError);
        }
        let ordinal = usize::try_from(row.inventory_ordinal).map_err(|_| ComponentTableError)?;
        let word = ordinal / u64::BITS as usize;
        let mask = 1_u64 << (ordinal % u64::BITS as usize);
        if self.inventory_ordinals[word] & mask != 0 {
            return Err(ComponentTableError);
        }
        self.inventory_ordinals[word] |= mask;
        let mut exact_prefix = String::new();
        for (depth, segment) in segments.iter().enumerate() {
            if depth > 0 {
                exact_prefix.push('/');
            }
            exact_prefix.push_str(segment);
            let portable_prefix = PortableRelativePath::new(&exact_prefix)
                .map_err(|_| ComponentTableError)?
                .key();
            let is_file = depth + 1 == segments.len();
            if let Some(existing) = self.projected_prefixes.get(&portable_prefix) {
                if existing.exact != exact_prefix || existing.is_file || is_file {
                    return Err(ComponentTableError);
                }
            } else {
                self.projected_prefixes
                    .try_reserve(1)
                    .map_err(|_| ComponentTableError)?;
                self.projected_prefix_path_bytes = self
                    .projected_prefix_path_bytes
                    .checked_add(exact_prefix.len())
                    .filter(|bytes| *bytes <= MAX_PROJECTED_PREFIX_PATH_BYTES)
                    .ok_or(ComponentTableError)?;
                self.projected_prefix_allocation_bytes = self
                    .projected_prefix_allocation_bytes
                    .checked_add(
                        exact_prefix
                            .len()
                            .checked_mul(2)
                            .ok_or(ComponentTableError)?,
                    )
                    .filter(|bytes| *bytes <= MAX_PROJECTED_PREFIX_ALLOCATION_BYTES)
                    .ok_or(ComponentTableError)?;
                if self.projected_prefixes.len() >= MAX_PROJECTED_PATH_PREFIXES {
                    return Err(ComponentTableError);
                }
                self.projected_prefixes.insert(
                    portable_prefix,
                    ProjectedPathPrefix {
                        exact: exact_prefix.clone(),
                        is_file,
                    },
                );
            }
        }
        self.created_depth_checks
            .push((portable_path, row.first_created_depth));
        let common_segments = self
            .last_path
            .as_deref()
            .map(|last| {
                last.split('/')
                    .zip(segments.iter().copied())
                    .take_while(|(left, right)| left == right)
                    .count()
            })
            .unwrap_or(0);
        if self.last_path.as_deref().is_some_and(|last| {
            row.path
                .as_str()
                .strip_prefix(last)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(ComponentTableError);
        }
        self.fanout_counts.truncate(common_segments + 1);
        for depth in common_segments..segments.len() {
            if depth == self.fanout_counts.len() {
                self.fanout_counts
                    .try_reserve(1)
                    .map_err(|_| ComponentTableError)?;
                self.fanout_counts.push(0);
            }
            self.fanout_counts[depth] = self.fanout_counts[depth]
                .checked_add(1)
                .filter(|count| *count <= MAX_MANAGED_DIRECTORY_ENTRIES)
                .ok_or(ComponentTableError)?;
        }

        let encoded_segment_count =
            u16::try_from(segments.len()).map_err(|_| ComponentTableError)?;
        if encoded_segment_count == 0 {
            return Err(ComponentTableError);
        }
        if row.prior.is_some() && row.first_created_depth.is_some() {
            return Err(ComponentTableError);
        }
        if let Some(depth) = row.first_created_depth {
            let parent_count = segments.len();
            if usize::from(depth) >= parent_count {
                return Err(ComponentTableError);
            }
            for parent_depth in usize::from(depth)..parent_count {
                let ancestor = created_ancestor_at_depth(&segments, parent_depth)?;
                let key = created_ancestor_key(&ancestor);
                if !self.created_ancestors.contains_key(&key) {
                    self.created_ancestors
                        .try_reserve(1)
                        .map_err(|_| ComponentTableError)?;
                    let path_len = created_ancestor_path_len(&ancestor);
                    self.created_ancestor_path_bytes = self
                        .created_ancestor_path_bytes
                        .checked_add(path_len)
                        .filter(|bytes| *bytes <= MAX_CREATED_ANCESTOR_PATH_BYTES)
                        .ok_or(ComponentTableError)?;
                    self.created_ancestor_allocation_bytes = self
                        .created_ancestor_allocation_bytes
                        .checked_add(path_len.checked_mul(2).ok_or(ComponentTableError)?)
                        .filter(|bytes| *bytes <= MAX_CREATED_ANCESTOR_ALLOCATION_BYTES)
                        .ok_or(ComponentTableError)?;
                    if self.created_ancestors.len() >= MAX_CREATED_ANCESTORS {
                        return Err(ComponentTableError);
                    }
                    self.created_ancestors.insert(key, ancestor);
                }
            }
        }

        self.final_bytes = checked_aggregate(self.final_bytes, row.final_size)?;
        self.staged_bytes = checked_aggregate(self.staged_bytes, row.staged_bytes())?;
        self.prior_bytes = checked_aggregate(self.prior_bytes, row.quarantine_bytes())?;
        self.row_count += 1;
        let mut last_path = String::new();
        last_path
            .try_reserve_exact(row.path.as_str().len())
            .map_err(|_| ComponentTableError)?;
        last_path.push_str(row.path.as_str());
        self.last_path = Some(last_path);
        Ok(())
    }

    fn validate_created_depths(&self) -> Result<(), ComponentTableError> {
        for (path, encoded_depth) in &self.created_depth_checks {
            let mut expected_depth = self.created_ancestors.contains_key("").then_some(0_u16);
            if expected_depth.is_none() {
                let segment_count = path.split('/').count();
                let mut prefix = String::new();
                prefix
                    .try_reserve_exact(path.len())
                    .map_err(|_| ComponentTableError)?;
                for (index, segment) in path.split('/').take(segment_count - 1).enumerate() {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(segment);
                    if self.created_ancestors.contains_key(&prefix) {
                        expected_depth =
                            Some(u16::try_from(index + 1).map_err(|_| ComponentTableError)?);
                        break;
                    }
                }
            }
            if *encoded_depth != expected_depth {
                return Err(ComponentTableError);
            }
        }
        Ok(())
    }
}

fn created_ancestor_at_depth(
    segments: &[&str],
    depth: usize,
) -> Result<ComponentCreatedAncestor, ComponentTableError> {
    if depth == 0 {
        return Ok(ComponentCreatedAncestor::ComponentRoot);
    }
    let path = segments.get(..depth).ok_or(ComponentTableError)?.join("/");
    Ok(ComponentCreatedAncestor::Relative(
        PortableRelativePath::new(&path).map_err(|_| ComponentTableError)?,
    ))
}

fn created_ancestor_key(ancestor: &ComponentCreatedAncestor) -> Option<PortablePathKey> {
    match ancestor {
        ComponentCreatedAncestor::ComponentRoot => None,
        ComponentCreatedAncestor::Relative(path) => Some(path.key()),
    }
}

fn created_ancestor_path_len(ancestor: &ComponentCreatedAncestor) -> usize {
    match ancestor {
        ComponentCreatedAncestor::ComponentRoot => 0,
        ComponentCreatedAncestor::Relative(path) => path.as_str().len(),
    }
}

fn checked_aggregate(current: u64, additional: u64) -> Result<u64, ComponentTableError> {
    current
        .checked_add(additional)
        .filter(|total| *total <= MAX_TIER2_AGGREGATE_BYTES)
        .ok_or(ComponentTableError)
}

pub(crate) fn encode_component_table_shard(
    shard: &ComponentTableShard,
) -> Result<Vec<u8>, ComponentTableError> {
    validate_shard_geometry(shard)?;
    let mut validator = ComponentRowValidator::new(shard.rows.len())?;
    let mut records_len = 0_usize;
    for row in &shard.rows {
        validator.push(shard.component, row)?;
        records_len = records_len
            .checked_add(encoded_row_len(row)?)
            .ok_or(ComponentTableError)?;
    }
    let mut records = Vec::new();
    records
        .try_reserve_exact(records_len)
        .map_err(|_| ComponentTableError)?;
    for row in &shard.rows {
        encode_row(row, &mut records)?;
    }
    let records_len = u32::try_from(records.len()).map_err(|_| ComponentTableError)?;
    let capacity = COMPONENT_TABLE_HEADER_BYTES
        .checked_add(records.len())
        .ok_or(ComponentTableError)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ComponentTableError)?;
    bytes.extend_from_slice(TABLE_MAGIC);
    put_u16(&mut bytes, FORMAT_VERSION);
    bytes.push(shard.component as u8);
    bytes.push(0);
    put_u32(&mut bytes, shard.shard_index);
    put_u32(&mut bytes, shard.shard_count);
    put_u32(&mut bytes, shard.first_row);
    put_u32(
        &mut bytes,
        u32::try_from(shard.rows.len()).map_err(|_| ComponentTableError)?,
    );
    put_u32(&mut bytes, shard.total_rows);
    bytes.extend_from_slice(&shard.transaction_nonce);
    bytes.extend_from_slice(&shard.root_binding_sha256);
    put_u64(&mut bytes, validator.final_bytes);
    put_u64(&mut bytes, validator.prior_bytes);
    put_u32(&mut bytes, records_len);
    put_u32(&mut bytes, 0);
    if bytes.len() != COMPONENT_TABLE_HEADER_BYTES {
        return Err(ComponentTableError);
    }
    bytes.extend_from_slice(&records);
    Ok(bytes)
}

fn encode_row(row: &ComponentTableRow, output: &mut Vec<u8>) -> Result<(), ComponentTableError> {
    let path = row.path.as_str().as_bytes();
    let record_len = encoded_row_len(row)?;
    put_u16(
        output,
        u16::try_from(record_len).map_err(|_| ComponentTableError)?,
    );
    put_u16(
        output,
        u16::try_from(path.len()).map_err(|_| ComponentTableError)?,
    );
    put_u32(output, row.inventory_ordinal);
    put_u64(output, row.final_size);
    output.extend_from_slice(&row.final_sha1);
    output.push(row.kind as u8);
    output.push(if row.prior.is_some() {
        PRIOR_PRESENT
    } else {
        0
    });
    put_u16(
        output,
        row.first_created_depth.unwrap_or(NO_CREATED_ANCESTOR),
    );
    put_u16(
        output,
        u16::try_from(row.path.as_str().split('/').count()).map_err(|_| ComponentTableError)?,
    );
    put_u16(output, 0);
    output.extend_from_slice(path);
    if let Some(prior) = &row.prior {
        put_u64(output, prior.size);
        output.extend_from_slice(&prior.sha1);
    }
    Ok(())
}

fn encoded_row_len(row: &ComponentTableRow) -> Result<usize, ComponentTableError> {
    COMPONENT_TABLE_ROW_PREFIX_BYTES
        .checked_add(row.path.as_str().len())
        .and_then(|length| length.checked_add(if row.prior.is_some() { 28 } else { 0 }))
        .filter(|length| *length <= u16::MAX as usize)
        .ok_or(ComponentTableError)
}

pub(crate) fn decode_component_table_shard(
    bytes: &[u8],
) -> Result<ComponentTableShard, ComponentTableError> {
    if bytes.len() < COMPONENT_TABLE_HEADER_BYTES
        || bytes.len() > MAX_COMPONENT_TABLE_SHARD_BYTES
        || &bytes[..8] != TABLE_MAGIC
    {
        return Err(ComponentTableError);
    }
    let mut cursor = ByteCursor::new(bytes);
    cursor.expect(TABLE_MAGIC)?;
    if cursor.u16()? != FORMAT_VERSION {
        return Err(ComponentTableError);
    }
    let component = ManagedComponentKind::from_byte(cursor.u8()?)?;
    if cursor.u8()? != 0 {
        return Err(ComponentTableError);
    }
    let shard_index = cursor.u32()?;
    let shard_count = cursor.u32()?;
    let first_row = cursor.u32()?;
    let row_count = usize::try_from(cursor.u32()?).map_err(|_| ComponentTableError)?;
    let total_rows = cursor.u32()?;
    let transaction_nonce = cursor.array::<16>()?;
    let root_binding_sha256 = cursor.array::<32>()?;
    let final_bytes = cursor.u64()?;
    let prior_bytes = cursor.u64()?;
    let records_len = usize::try_from(cursor.u32()?).map_err(|_| ComponentTableError)?;
    if cursor.u32()? != 0
        || cursor.position() != COMPONENT_TABLE_HEADER_BYTES
        || records_len != bytes.len() - COMPONENT_TABLE_HEADER_BYTES
        || row_count > COMPONENT_TABLE_ROWS_PER_SHARD
    {
        return Err(ComponentTableError);
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count)
        .map_err(|_| ComponentTableError)?;
    for _ in 0..row_count {
        rows.push(decode_row(&mut cursor)?);
    }
    if cursor.position() != bytes.len() {
        return Err(ComponentTableError);
    }
    let shard = ComponentTableShard {
        component,
        shard_index,
        shard_count,
        first_row,
        total_rows,
        transaction_nonce,
        root_binding_sha256,
        rows,
    };
    validate_shard_geometry(&shard)?;
    let mut validator = ComponentRowValidator::new(shard.rows.len())?;
    for row in &shard.rows {
        validator.push(component, row)?;
    }
    if validator.final_bytes != final_bytes || validator.prior_bytes != prior_bytes {
        return Err(ComponentTableError);
    }
    Ok(shard)
}

fn decode_row(cursor: &mut ByteCursor<'_>) -> Result<ComponentTableRow, ComponentTableError> {
    let record_start = cursor.position();
    let record_len = usize::from(cursor.u16()?);
    let path_len = usize::from(cursor.u16()?);
    if record_len < COMPONENT_TABLE_ROW_PREFIX_BYTES
        || path_len == 0
        || path_len > MAX_COMPONENT_PATH_BYTES
        || record_start
            .checked_add(record_len)
            .is_none_or(|end| end > cursor.bytes.len())
    {
        return Err(ComponentTableError);
    }
    let inventory_ordinal = cursor.u32()?;
    let final_size = cursor.u64()?;
    let final_sha1 = cursor.array::<20>()?;
    let kind = ManagedComponentArtifactKind::from_byte(cursor.u8()?)?;
    let flags = cursor.u8()?;
    if flags & !PRIOR_PRESENT != 0 {
        return Err(ComponentTableError);
    }
    let first_created_depth = match cursor.u16()? {
        NO_CREATED_ANCESTOR => None,
        depth => Some(depth),
    };
    let segment_count = cursor.u16()?;
    if cursor.u16()? != 0 {
        return Err(ComponentTableError);
    }
    let expected_record_len = COMPONENT_TABLE_ROW_PREFIX_BYTES
        .checked_add(path_len)
        .and_then(|length| length.checked_add(if flags & PRIOR_PRESENT != 0 { 28 } else { 0 }))
        .ok_or(ComponentTableError)?;
    if record_len != expected_record_len {
        return Err(ComponentTableError);
    }
    let path_bytes = cursor.take(path_len)?;
    let path_text = std::str::from_utf8(path_bytes).map_err(|_| ComponentTableError)?;
    let path = PortableRelativePath::new(path_text).map_err(|_| ComponentTableError)?;
    if path.as_str().as_bytes() != path_bytes
        || usize::from(segment_count) != path.as_str().split('/').count()
    {
        return Err(ComponentTableError);
    }
    let prior = if flags & PRIOR_PRESENT != 0 {
        Some(ComponentPriorFile {
            size: cursor.u64()?,
            sha1: cursor.array::<20>()?,
        })
    } else {
        None
    };
    if cursor.position() != record_start + record_len {
        return Err(ComponentTableError);
    }
    Ok(ComponentTableRow {
        inventory_ordinal,
        final_size,
        final_sha1,
        kind,
        path,
        first_created_depth,
        prior,
    })
}

fn validate_shard_geometry(shard: &ComponentTableShard) -> Result<(), ComponentTableError> {
    let total_rows = usize::try_from(shard.total_rows).map_err(|_| ComponentTableError)?;
    let shard_count = expected_shard_count(total_rows)?;
    let shard_index = usize::try_from(shard.shard_index).map_err(|_| ComponentTableError)?;
    if total_rows == 0
        || usize::try_from(shard.shard_count).map_err(|_| ComponentTableError)? != shard_count
        || shard_index >= shard_count
        || usize::try_from(shard.first_row).map_err(|_| ComponentTableError)?
            != shard_index
                .checked_mul(COMPONENT_TABLE_ROWS_PER_SHARD)
                .ok_or(ComponentTableError)?
        || shard.rows.len() != expected_shard_row_count(total_rows, shard_index)?
    {
        return Err(ComponentTableError);
    }
    Ok(())
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ComponentTableError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(ComponentTableError)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn expect(&mut self, expected: &[u8]) -> Result<(), ComponentTableError> {
        if self.take(expected.len())? != expected {
            return Err(ComponentTableError);
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ComponentTableError> {
        self.take(N)?.try_into().map_err(|_| ComponentTableError)
    }

    fn u8(&mut self) -> Result<u8, ComponentTableError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, ComponentTableError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ComponentTableError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ComponentTableError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

pub(crate) fn encode_component_intent_manifest(
    manifest: &ComponentIntentManifest,
) -> Result<Vec<u8>, ComponentTableError> {
    validate_manifest(manifest)?;
    let descriptor_bytes = manifest
        .shards
        .len()
        .checked_mul(COMPONENT_INTENT_DESCRIPTOR_BYTES)
        .ok_or(ComponentTableError)?;
    let total_bytes = COMPONENT_INTENT_HEADER_BYTES
        .checked_add(descriptor_bytes)
        .filter(|length| *length <= MAX_COMPONENT_INTENT_BYTES)
        .ok_or(ComponentTableError)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_bytes)
        .map_err(|_| ComponentTableError)?;
    bytes.extend_from_slice(INTENT_MAGIC);
    put_u16(&mut bytes, FORMAT_VERSION);
    bytes.push(manifest.component as u8);
    bytes.push(0);
    put_u32(
        &mut bytes,
        u32::try_from(manifest.shards.len()).map_err(|_| ComponentTableError)?,
    );
    put_u32(&mut bytes, manifest.total_rows);
    put_u16(
        &mut bytes,
        u16::try_from(COMPONENT_INTENT_DESCRIPTOR_BYTES).map_err(|_| ComponentTableError)?,
    );
    put_u16(&mut bytes, 0);
    put_u64(&mut bytes, manifest.final_bytes);
    put_u64(&mut bytes, manifest.prior_bytes);
    bytes.extend_from_slice(&manifest.transaction_nonce);
    bytes.extend_from_slice(&manifest.root_binding_sha256);
    bytes.extend_from_slice(&manifest.logical_rows_sha256);
    bytes.extend_from_slice(&manifest.projection_sha256);
    put_u32(
        &mut bytes,
        u32::try_from(descriptor_bytes).map_err(|_| ComponentTableError)?,
    );
    put_u32(&mut bytes, 0);
    if bytes.len() != COMPONENT_INTENT_HEADER_BYTES {
        return Err(ComponentTableError);
    }
    for descriptor in &manifest.shards {
        put_u32(&mut bytes, descriptor.shard_index);
        put_u32(&mut bytes, descriptor.first_row);
        put_u32(&mut bytes, descriptor.row_count);
        put_u32(&mut bytes, descriptor.byte_len);
        put_u64(&mut bytes, descriptor.final_bytes);
        put_u64(&mut bytes, descriptor.prior_bytes);
        bytes.extend_from_slice(&descriptor.sha256);
    }
    if bytes.len() != total_bytes {
        return Err(ComponentTableError);
    }
    Ok(bytes)
}

pub(crate) fn decode_component_intent_manifest(
    bytes: &[u8],
) -> Result<ComponentIntentManifest, ComponentTableError> {
    if bytes.len() < COMPONENT_INTENT_HEADER_BYTES
        || bytes.len() > MAX_COMPONENT_INTENT_BYTES
        || &bytes[..8] != INTENT_MAGIC
    {
        return Err(ComponentTableError);
    }
    let mut cursor = ByteCursor::new(bytes);
    cursor.expect(INTENT_MAGIC)?;
    if cursor.u16()? != FORMAT_VERSION {
        return Err(ComponentTableError);
    }
    let component = ManagedComponentKind::from_byte(cursor.u8()?)?;
    if cursor.u8()? != 0 {
        return Err(ComponentTableError);
    }
    let shard_count = usize::try_from(cursor.u32()?).map_err(|_| ComponentTableError)?;
    let total_rows = cursor.u32()?;
    if usize::from(cursor.u16()?) != COMPONENT_INTENT_DESCRIPTOR_BYTES || cursor.u16()? != 0 {
        return Err(ComponentTableError);
    }
    let final_bytes = cursor.u64()?;
    let prior_bytes = cursor.u64()?;
    let transaction_nonce = cursor.array::<16>()?;
    let root_binding_sha256 = cursor.array::<32>()?;
    let logical_rows_sha256 = cursor.array::<32>()?;
    let projection_sha256 = cursor.array::<32>()?;
    let descriptors_len = usize::try_from(cursor.u32()?).map_err(|_| ComponentTableError)?;
    if cursor.u32()? != 0
        || cursor.position() != COMPONENT_INTENT_HEADER_BYTES
        || shard_count > MAX_COMPONENT_TABLE_SHARDS
        || descriptors_len
            != shard_count
                .checked_mul(COMPONENT_INTENT_DESCRIPTOR_BYTES)
                .ok_or(ComponentTableError)?
        || bytes.len() != COMPONENT_INTENT_HEADER_BYTES + descriptors_len
    {
        return Err(ComponentTableError);
    }
    let mut shards = Vec::new();
    shards
        .try_reserve_exact(shard_count)
        .map_err(|_| ComponentTableError)?;
    for _ in 0..shard_count {
        shards.push(ComponentShardDescriptor {
            shard_index: cursor.u32()?,
            first_row: cursor.u32()?,
            row_count: cursor.u32()?,
            byte_len: cursor.u32()?,
            final_bytes: cursor.u64()?,
            prior_bytes: cursor.u64()?,
            sha256: cursor.array::<32>()?,
        });
    }
    if cursor.position() != bytes.len() {
        return Err(ComponentTableError);
    }
    let manifest = ComponentIntentManifest {
        component,
        total_rows,
        final_bytes,
        prior_bytes,
        transaction_nonce,
        root_binding_sha256,
        logical_rows_sha256,
        projection_sha256,
        shards,
    };
    validate_manifest(&manifest)?;
    if encode_component_intent_manifest(&manifest)? != bytes {
        return Err(ComponentTableError);
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &ComponentIntentManifest) -> Result<(), ComponentTableError> {
    let total_rows = usize::try_from(manifest.total_rows).map_err(|_| ComponentTableError)?;
    let expected_count = expected_shard_count(total_rows)?;
    if manifest.shards.len() != expected_count
        || manifest.final_bytes > MAX_TIER2_AGGREGATE_BYTES
        || manifest.prior_bytes > MAX_TIER2_AGGREGATE_BYTES
    {
        return Err(ComponentTableError);
    }
    let mut final_bytes = 0_u64;
    let mut prior_bytes = 0_u64;
    for (index, descriptor) in manifest.shards.iter().enumerate() {
        let expected_first = index
            .checked_mul(COMPONENT_TABLE_ROWS_PER_SHARD)
            .ok_or(ComponentTableError)?;
        let expected_rows = expected_shard_row_count(total_rows, index)?;
        if usize::try_from(descriptor.shard_index).map_err(|_| ComponentTableError)? != index
            || usize::try_from(descriptor.first_row).map_err(|_| ComponentTableError)?
                != expected_first
            || usize::try_from(descriptor.row_count).map_err(|_| ComponentTableError)?
                != expected_rows
            || usize::try_from(descriptor.byte_len).map_err(|_| ComponentTableError)?
                < COMPONENT_TABLE_HEADER_BYTES
            || usize::try_from(descriptor.byte_len).map_err(|_| ComponentTableError)?
                > MAX_COMPONENT_TABLE_SHARD_BYTES
            || descriptor.final_bytes > MAX_TIER2_AGGREGATE_BYTES
            || descriptor.prior_bytes > MAX_TIER2_AGGREGATE_BYTES
        {
            return Err(ComponentTableError);
        }
        final_bytes = checked_aggregate(final_bytes, descriptor.final_bytes)?;
        prior_bytes = checked_aggregate(prior_bytes, descriptor.prior_bytes)?;
    }
    if final_bytes != manifest.final_bytes || prior_bytes != manifest.prior_bytes {
        return Err(ComponentTableError);
    }
    Ok(())
}

struct ComponentTableSequenceValidator {
    component: ManagedComponentKind,
    total_rows: u32,
    shard_count: usize,
    transaction_nonce: [u8; 16],
    root_binding_sha256: [u8; 32],
    next_shard: usize,
    rows: ComponentRowValidator,
    logical_rows_hasher: Sha256,
    projection_hasher: Sha256,
}

impl ComponentTableSequenceValidator {
    fn new(
        component: ManagedComponentKind,
        total_rows: u32,
        transaction_nonce: [u8; 16],
        root_binding_sha256: [u8; 32],
    ) -> Result<Self, ComponentTableError> {
        let shard_count =
            expected_shard_count(usize::try_from(total_rows).map_err(|_| ComponentTableError)?)?;
        let mut projection_hasher = Sha256::new();
        projection_hasher.update(b"axial.component.projection.v1\0");
        projection_hasher.update([component as u8]);
        projection_hasher.update(total_rows.to_le_bytes());
        let mut logical_rows_hasher = Sha256::new();
        logical_rows_hasher.update(b"axial.component.rows.v1\0");
        logical_rows_hasher.update([component as u8]);
        logical_rows_hasher.update(total_rows.to_le_bytes());
        Ok(Self {
            component,
            total_rows,
            shard_count,
            transaction_nonce,
            root_binding_sha256,
            next_shard: 0,
            rows: ComponentRowValidator::new(
                usize::try_from(total_rows).map_err(|_| ComponentTableError)?,
            )?,
            logical_rows_hasher,
            projection_hasher,
        })
    }

    fn push(
        &mut self,
        shard: &ComponentTableShard,
        encoded: &[u8],
    ) -> Result<(), ComponentTableError> {
        if shard.component != self.component
            || shard.total_rows != self.total_rows
            || usize::try_from(shard.shard_count).map_err(|_| ComponentTableError)?
                != self.shard_count
            || usize::try_from(shard.shard_index).map_err(|_| ComponentTableError)?
                != self.next_shard
            || shard.transaction_nonce != self.transaction_nonce
            || shard.root_binding_sha256 != self.root_binding_sha256
            || encoded.len() < COMPONENT_TABLE_HEADER_BYTES
        {
            return Err(ComponentTableError);
        }
        for row in &shard.rows {
            self.rows.push(self.component, row)?;
            update_projection_hash(&mut self.projection_hasher, row)?;
        }
        self.logical_rows_hasher
            .update(&encoded[COMPONENT_TABLE_HEADER_BYTES..]);
        self.next_shard += 1;
        Ok(())
    }

    fn finish(self) -> Result<ComponentTableSummary, ComponentTableError> {
        if self.next_shard != self.shard_count
            || self.rows.row_count
                != usize::try_from(self.total_rows).map_err(|_| ComponentTableError)?
        {
            return Err(ComponentTableError);
        }
        self.rows.validate_created_depths()?;
        let mut created_ancestors = self
            .rows
            .created_ancestors
            .into_values()
            .collect::<Vec<_>>();
        created_ancestors.sort();
        Ok(ComponentTableSummary {
            row_count: self.rows.row_count,
            final_bytes: self.rows.final_bytes,
            staged_bytes: self.rows.staged_bytes,
            prior_bytes: self.rows.prior_bytes,
            created_ancestors,
            logical_rows_sha256: self.logical_rows_hasher.finalize().into(),
            projection_sha256: self.projection_hasher.finalize().into(),
        })
    }
}

fn update_projection_hash(
    hasher: &mut Sha256,
    row: &ComponentTableRow,
) -> Result<(), ComponentTableError> {
    let path = row.path.as_str().as_bytes();
    hasher.update(row.inventory_ordinal.to_le_bytes());
    hasher.update([row.kind as u8]);
    hasher.update(row.final_size.to_le_bytes());
    hasher.update(row.final_sha1);
    hasher.update(
        u16::try_from(path.len())
            .map_err(|_| ComponentTableError)?
            .to_le_bytes(),
    );
    hasher.update(path);
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentTableSummary {
    pub(crate) row_count: usize,
    pub(crate) final_bytes: u64,
    pub(crate) staged_bytes: u64,
    pub(crate) prior_bytes: u64,
    pub(crate) created_ancestors: Vec<ComponentCreatedAncestor>,
    pub(crate) logical_rows_sha256: [u8; 32],
    pub(crate) projection_sha256: [u8; 32],
}

pub(crate) struct ComponentTableBuilder {
    sequence: ComponentTableSequenceValidator,
    descriptors: Vec<ComponentShardDescriptor>,
    poisoned: bool,
}

impl ComponentTableBuilder {
    pub(crate) fn new(
        component: ManagedComponentKind,
        total_rows: usize,
        transaction_nonce: [u8; 16],
        root_binding_sha256: [u8; 32],
    ) -> Result<Self, ComponentTableError> {
        let total_rows = u32::try_from(total_rows).map_err(|_| ComponentTableError)?;
        let sequence = ComponentTableSequenceValidator::new(
            component,
            total_rows,
            transaction_nonce,
            root_binding_sha256,
        )?;
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(sequence.shard_count)
            .map_err(|_| ComponentTableError)?;
        Ok(Self {
            sequence,
            descriptors,
            poisoned: false,
        })
    }

    pub(crate) fn push_shard(
        &mut self,
        rows: Vec<ComponentTableRow>,
    ) -> Result<(Vec<u8>, ComponentShardDescriptor), ComponentTableError> {
        if self.poisoned || self.sequence.next_shard >= self.sequence.shard_count {
            return Err(ComponentTableError);
        }
        let expected_rows = expected_shard_row_count(
            usize::try_from(self.sequence.total_rows).map_err(|_| ComponentTableError)?,
            self.sequence.next_shard,
        )?;
        if rows.len() != expected_rows || rows.len() > COMPONENT_TABLE_ROWS_PER_SHARD {
            return Err(ComponentTableError);
        }
        let shard_index = self.sequence.next_shard;
        let shard = ComponentTableShard {
            component: self.sequence.component,
            shard_index: u32::try_from(shard_index).map_err(|_| ComponentTableError)?,
            shard_count: u32::try_from(self.sequence.shard_count)
                .map_err(|_| ComponentTableError)?,
            first_row: u32::try_from(
                shard_index
                    .checked_mul(COMPONENT_TABLE_ROWS_PER_SHARD)
                    .ok_or(ComponentTableError)?,
            )
            .map_err(|_| ComponentTableError)?,
            total_rows: self.sequence.total_rows,
            transaction_nonce: self.sequence.transaction_nonce,
            root_binding_sha256: self.sequence.root_binding_sha256,
            rows,
        };
        let encoded = encode_component_table_shard(&shard)?;
        let (final_bytes, prior_bytes) = table_shard_totals(&shard)?;
        let descriptor = ComponentShardDescriptor {
            shard_index: shard.shard_index,
            first_row: shard.first_row,
            row_count: u32::try_from(shard.rows.len()).map_err(|_| ComponentTableError)?,
            byte_len: u32::try_from(encoded.len()).map_err(|_| ComponentTableError)?,
            final_bytes,
            prior_bytes,
            sha256: Sha256::digest(&encoded).into(),
        };

        self.poisoned = true;
        self.sequence.push(&shard, &encoded)?;
        self.descriptors.push(descriptor.clone());
        self.poisoned = false;
        Ok((encoded, descriptor))
    }

    pub(crate) fn finish(
        self,
    ) -> Result<(ComponentIntentManifest, ComponentTableSummary), ComponentTableError> {
        if self.poisoned {
            return Err(ComponentTableError);
        }
        let component = self.sequence.component;
        let total_rows = self.sequence.total_rows;
        let transaction_nonce = self.sequence.transaction_nonce;
        let root_binding_sha256 = self.sequence.root_binding_sha256;
        let summary = self.sequence.finish()?;
        let manifest = ComponentIntentManifest {
            component,
            total_rows,
            final_bytes: summary.final_bytes,
            prior_bytes: summary.prior_bytes,
            transaction_nonce,
            root_binding_sha256,
            logical_rows_sha256: summary.logical_rows_sha256,
            projection_sha256: summary.projection_sha256,
            shards: self.descriptors,
        };
        validate_manifest(&manifest)?;
        Ok((manifest, summary))
    }
}

#[cfg(test)]
pub(crate) fn build_component_intent_manifest(
    component: ManagedComponentKind,
    transaction_nonce: [u8; 16],
    root_binding_sha256: [u8; 32],
    encoded_shards: &[Vec<u8>],
) -> Result<(ComponentIntentManifest, ComponentTableSummary), ComponentTableError> {
    let total_rows = if let Some(first) = encoded_shards.first() {
        decode_component_table_shard(first)?.total_rows
    } else {
        0
    };
    let mut builder = ComponentTableBuilder::new(
        component,
        usize::try_from(total_rows).map_err(|_| ComponentTableError)?,
        transaction_nonce,
        root_binding_sha256,
    )?;
    for bytes in encoded_shards {
        let shard = decode_component_table_shard(bytes)?;
        let (rebuilt, _) = builder.push_shard(shard.rows)?;
        if rebuilt != *bytes {
            return Err(ComponentTableError);
        }
    }
    builder.finish()
}

fn table_shard_totals(shard: &ComponentTableShard) -> Result<(u64, u64), ComponentTableError> {
    let mut final_bytes = 0_u64;
    let mut prior_bytes = 0_u64;
    for row in &shard.rows {
        final_bytes = checked_aggregate(final_bytes, row.final_size)?;
        prior_bytes = checked_aggregate(prior_bytes, row.quarantine_bytes())?;
    }
    Ok((final_bytes, prior_bytes))
}

pub(crate) struct ComponentTableParser {
    manifest: ComponentIntentManifest,
    sequence: ComponentTableSequenceValidator,
}

impl ComponentTableParser {
    pub(crate) fn new(manifest: ComponentIntentManifest) -> Result<Self, ComponentTableError> {
        validate_manifest(&manifest)?;
        let sequence = ComponentTableSequenceValidator::new(
            manifest.component,
            manifest.total_rows,
            manifest.transaction_nonce,
            manifest.root_binding_sha256,
        )?;
        Ok(Self { manifest, sequence })
    }

    pub(crate) fn parse_next(
        &mut self,
        bytes: &[u8],
    ) -> Result<ComponentTableShard, ComponentTableError> {
        let descriptor = self
            .manifest
            .shards
            .get(self.sequence.next_shard)
            .ok_or(ComponentTableError)?;
        if usize::try_from(descriptor.byte_len).map_err(|_| ComponentTableError)? != bytes.len()
            || <[u8; 32]>::from(Sha256::digest(bytes)) != descriptor.sha256
        {
            return Err(ComponentTableError);
        }
        let shard = decode_component_table_shard(bytes)?;
        let (final_bytes, prior_bytes) = table_shard_totals(&shard)?;
        if descriptor.shard_index != shard.shard_index
            || descriptor.first_row != shard.first_row
            || usize::try_from(descriptor.row_count).map_err(|_| ComponentTableError)?
                != shard.rows.len()
            || descriptor.final_bytes != final_bytes
            || descriptor.prior_bytes != prior_bytes
        {
            return Err(ComponentTableError);
        }
        self.sequence.push(&shard, bytes)?;
        Ok(shard)
    }

    pub(crate) fn finish(self) -> Result<ComponentTableSummary, ComponentTableError> {
        let summary = self.sequence.finish()?;
        if summary.final_bytes != self.manifest.final_bytes
            || summary.prior_bytes != self.manifest.prior_bytes
            || summary.logical_rows_sha256 != self.manifest.logical_rows_sha256
            || summary.projection_sha256 != self.manifest.projection_sha256
        {
            return Err(ComponentTableError);
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn digest_hex(value: [u8; 32]) -> String {
        value
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn path(value: &str) -> PortableRelativePath {
        PortableRelativePath::new(value).expect("test component path")
    }

    fn row(
        inventory_ordinal: u32,
        value: &str,
        kind: ManagedComponentArtifactKind,
        final_size: u64,
    ) -> ComponentTableRow {
        ComponentTableRow {
            inventory_ordinal,
            final_size,
            final_sha1: digest(inventory_ordinal as u8),
            kind,
            path: path(value),
            first_created_depth: None,
            prior: None,
        }
    }

    fn asset_rows(start: u32, end: u32) -> Vec<ComponentTableRow> {
        (start..end)
            .map(|index| {
                row(
                    index,
                    &format!("objects/{index:06}"),
                    ManagedComponentArtifactKind::AssetObject,
                    1,
                )
            })
            .collect()
    }

    fn shard(
        component: ManagedComponentKind,
        shard_index: u32,
        total_rows: u32,
        rows: Vec<ComponentTableRow>,
    ) -> ComponentTableShard {
        ComponentTableShard {
            component,
            shard_index,
            shard_count: u32::try_from(
                expected_shard_count(total_rows as usize).expect("test shard count"),
            )
            .unwrap(),
            first_row: shard_index * COMPONENT_TABLE_ROWS_PER_SHARD as u32,
            total_rows,
            transaction_nonce: [0x11; 16],
            root_binding_sha256: [0x22; 32],
            rows,
        }
    }

    fn encoded_three_row_table() -> Vec<u8> {
        let mut absent = row(0, "a/absent.jar", ManagedComponentArtifactKind::Library, 10);
        absent.first_created_depth = Some(1);
        let mut exact = row(
            1,
            "b/exact.jar",
            ManagedComponentArtifactKind::NativeLibrary,
            20,
        );
        exact.prior = Some(ComponentPriorFile {
            size: 20,
            sha1: exact.final_sha1,
        });
        let mut replacement = row(
            2,
            "c/replacement.jar",
            ManagedComponentArtifactKind::Library,
            30,
        );
        replacement.prior = Some(ComponentPriorFile {
            size: 25,
            sha1: digest(0xee),
        });
        encode_component_table_shard(&shard(
            ManagedComponentKind::Libraries,
            0,
            3,
            vec![absent, exact, replacement],
        ))
        .expect("encode table")
    }

    #[test]
    fn fixed_layout_sizes_and_slot_names_are_exact() {
        assert_eq!(COMPONENT_TABLE_HEADER_BYTES, 104);
        assert_eq!(COMPONENT_TABLE_ROW_PREFIX_BYTES, 44);
        assert_eq!(MAX_COMPONENT_TABLE_SHARDS, 782);
        assert_eq!(MAX_COMPONENT_TABLE_SHARD_BYTES, 149_608);
        assert_eq!(COMPONENT_INTENT_HEADER_BYTES, 160);
        assert_eq!(COMPONENT_INTENT_DESCRIPTOR_BYTES, 64);
        assert_eq!(MAX_COMPONENT_INTENT_BYTES, 50_208);
        assert_eq!(component_table_path(0).unwrap(), "table/000000.tbl");
        assert_eq!(component_table_path(781).unwrap(), "table/000781.tbl");
        assert_eq!(component_entry_slot(0, 0).unwrap(), "000000/000");
        assert_eq!(component_entry_slot(781, 255).unwrap(), "000781/255");
        assert_eq!(component_table_path(782), Err(ComponentTableError));
        assert_eq!(component_entry_slot(0, 256), Err(ComponentTableError));
    }

    #[test]
    fn table_roundtrip_derives_source_prior_and_created_ancestor_state() {
        let bytes = encoded_three_row_table();
        let decoded = decode_component_table_shard(&bytes).expect("decode table");
        assert_eq!(encode_component_table_shard(&decoded).unwrap(), bytes);
        assert_eq!(u64::from_le_bytes(bytes[80..88].try_into().unwrap()), 60);
        assert_eq!(u64::from_le_bytes(bytes[88..96].try_into().unwrap()), 25);

        let (manifest, summary) = build_component_intent_manifest(
            ManagedComponentKind::Libraries,
            [0x11; 16],
            [0x22; 32],
            &[bytes],
        )
        .expect("build intent");
        assert_eq!(manifest.final_bytes, 60);
        assert_eq!(manifest.prior_bytes, 25);
        assert_eq!(summary.staged_bytes, 40);
        assert_eq!(
            summary.created_ancestors,
            vec![ComponentCreatedAncestor::Relative(path("a"))]
        );
    }

    #[test]
    fn table_parser_rejects_every_truncation_and_noncanonical_header_fields() {
        let bytes = encoded_three_row_table();
        for length in 0..bytes.len() {
            assert_eq!(
                decode_component_table_shard(&bytes[..length]),
                Err(ComponentTableError),
                "accepted truncation at {length}",
            );
        }
        for (offset, replacement) in [(8, 2_u8), (11, 1), (100, 1)] {
            let mut corrupted = bytes.clone();
            corrupted[offset] = replacement;
            assert_eq!(
                decode_component_table_shard(&corrupted),
                Err(ComponentTableError),
                "accepted noncanonical byte at {offset}",
            );
        }
        let mut overflow = bytes.clone();
        overflow[96..100].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_component_table_shard(&overflow),
            Err(ComponentTableError)
        );
        let oversized = vec![0; MAX_COMPONENT_TABLE_SHARD_BYTES + 1];
        assert_eq!(
            decode_component_table_shard(&oversized),
            Err(ComponentTableError)
        );
    }

    #[test]
    fn row_parser_rejects_noncanonical_flags_reserved_and_segment_count() {
        let original = encode_component_table_shard(&shard(
            ManagedComponentKind::Libraries,
            0,
            1,
            vec![row(
                0,
                "a/library.jar",
                ManagedComponentArtifactKind::Library,
                1,
            )],
        ))
        .unwrap();
        for (row_offset, replacement) in [(37, 2_u8), (40, 9), (42, 1)] {
            let mut corrupted = original.clone();
            corrupted[COMPONENT_TABLE_HEADER_BYTES + row_offset] = replacement;
            assert_eq!(
                decode_component_table_shard(&corrupted),
                Err(ComponentTableError),
                "accepted row byte at {row_offset}",
            );
        }
    }

    #[test]
    fn codec_rejects_wrong_component_portable_alias_order_and_path_tree() {
        let wrong_component = shard(
            ManagedComponentKind::Assets,
            0,
            1,
            vec![row(
                0,
                "library.jar",
                ManagedComponentArtifactKind::Library,
                1,
            )],
        );
        assert_eq!(
            encode_component_table_shard(&wrong_component),
            Err(ComponentTableError)
        );

        for rows in [
            vec![
                row(0, "A/file", ManagedComponentArtifactKind::Library, 1),
                row(1, "a/FILE", ManagedComponentArtifactKind::Library, 1),
            ],
            vec![
                row(0, "b/file", ManagedComponentArtifactKind::Library, 1),
                row(1, "a/file", ManagedComponentArtifactKind::Library, 1),
            ],
            vec![
                row(0, "a", ManagedComponentArtifactKind::Library, 1),
                row(1, "a/file", ManagedComponentArtifactKind::Library, 1),
            ],
        ] {
            assert_eq!(
                encode_component_table_shard(&shard(ManagedComponentKind::Libraries, 0, 2, rows,)),
                Err(ComponentTableError),
            );
        }
        assert_eq!(
            encode_component_table_shard(&shard(
                ManagedComponentKind::Libraries,
                0,
                1,
                vec![row(
                    0,
                    "CON/library.jar",
                    ManagedComponentArtifactKind::Library,
                    1,
                )],
            )),
            Err(ComponentTableError),
        );
    }

    #[test]
    fn prior_size_accepts_the_tier_two_limit_and_rejects_limit_plus_one() {
        let mut bounded = row(0, "a/library.jar", ManagedComponentArtifactKind::Library, 1);
        bounded.prior = Some(ComponentPriorFile {
            size: MAX_TIER2_ARTIFACT_BYTES,
            sha1: digest(8),
        });
        assert!(
            encode_component_table_shard(&shard(
                ManagedComponentKind::Libraries,
                0,
                1,
                vec![bounded.clone()],
            ))
            .is_ok()
        );
        bounded.prior.as_mut().unwrap().size = MAX_TIER2_ARTIFACT_BYTES + 1;
        assert_eq!(
            encode_component_table_shard(&shard(
                ManagedComponentKind::Libraries,
                0,
                1,
                vec![bounded],
            )),
            Err(ComponentTableError),
        );
    }

    #[test]
    fn global_validation_rejects_cross_shard_file_ancestor_and_portable_prefix_aliases() {
        let mut rows = (0..255_u32)
            .map(|index| {
                row(
                    index,
                    &format!("{index:06}.jar"),
                    ManagedComponentArtifactKind::Library,
                    0,
                )
            })
            .collect::<Vec<_>>();
        rows.push(row(255, "a", ManagedComponentArtifactKind::Library, 0));
        let tables = vec![
            encode_component_table_shard(&shard(ManagedComponentKind::Libraries, 0, 257, rows))
                .unwrap(),
            encode_component_table_shard(&shard(
                ManagedComponentKind::Libraries,
                1,
                257,
                vec![row(
                    256,
                    "a/child.jar",
                    ManagedComponentArtifactKind::Library,
                    0,
                )],
            ))
            .unwrap(),
        ];
        assert_eq!(
            build_component_intent_manifest(
                ManagedComponentKind::Libraries,
                [0x11; 16],
                [0x22; 32],
                &tables,
            ),
            Err(ComponentTableError),
        );

        let aliases = encode_component_table_shard(&shard(
            ManagedComponentKind::Libraries,
            0,
            2,
            vec![
                row(0, "A/file.jar", ManagedComponentArtifactKind::Library, 0),
                row(1, "a", ManagedComponentArtifactKind::Library, 0),
            ],
        ));
        assert_eq!(aliases, Err(ComponentTableError));
    }

    #[test]
    fn created_depth_uses_component_root_zero_and_is_globally_canonical() {
        let mut first = row(0, "a/first.jar", ManagedComponentArtifactKind::Library, 1);
        first.first_created_depth = Some(0);
        let mut second = row(1, "b/second.jar", ManagedComponentArtifactKind::Library, 1);
        second.first_created_depth = Some(0);
        let root_table = encode_component_table_shard(&shard(
            ManagedComponentKind::Libraries,
            0,
            2,
            vec![first, second],
        ))
        .unwrap();
        let (_, summary) = build_component_intent_manifest(
            ManagedComponentKind::Libraries,
            [0x11; 16],
            [0x22; 32],
            &[root_table],
        )
        .unwrap();
        assert_eq!(
            summary.created_ancestors,
            vec![
                ComponentCreatedAncestor::ComponentRoot,
                ComponentCreatedAncestor::Relative(path("a")),
                ComponentCreatedAncestor::Relative(path("b")),
            ]
        );

        let mut created = row(0, "a/first.jar", ManagedComponentArtifactKind::Library, 1);
        created.first_created_depth = Some(1);
        let overlapping = row(1, "a/second.jar", ManagedComponentArtifactKind::Library, 1);
        let alternate = encode_component_table_shard(&shard(
            ManagedComponentKind::Libraries,
            0,
            2,
            vec![created, overlapping],
        ))
        .unwrap();
        assert_eq!(
            build_component_intent_manifest(
                ManagedComponentKind::Libraries,
                [0x11; 16],
                [0x22; 32],
                &[alternate],
            ),
            Err(ComponentTableError),
        );

        let mut prior = row(0, "a/prior.jar", ManagedComponentArtifactKind::Library, 1);
        prior.prior = Some(ComponentPriorFile {
            size: 2,
            sha1: digest(9),
        });
        prior.first_created_depth = Some(1);
        assert_eq!(
            encode_component_table_shard(&shard(
                ManagedComponentKind::Libraries,
                0,
                1,
                vec![prior],
            )),
            Err(ComponentTableError),
        );
    }

    #[test]
    fn validator_enforces_parent_fanout_and_ordinal_bitmap() {
        let mut validator = ComponentRowValidator::new(MAX_MANAGED_DIRECTORY_ENTRIES + 1).unwrap();
        for index in 0..MAX_MANAGED_DIRECTORY_ENTRIES {
            validator
                .push(
                    ManagedComponentKind::Libraries,
                    &row(
                        index as u32,
                        &format!("{index:04}.jar"),
                        ManagedComponentArtifactKind::Library,
                        0,
                    ),
                )
                .unwrap();
        }
        assert_eq!(
            validator.push(
                ManagedComponentKind::Libraries,
                &row(
                    MAX_MANAGED_DIRECTORY_ENTRIES as u32,
                    "zzzz.jar",
                    ManagedComponentArtifactKind::Library,
                    0,
                ),
            ),
            Err(ComponentTableError),
        );

        let mut ordinals = ComponentRowValidator::new(2).unwrap();
        ordinals
            .push(
                ManagedComponentKind::Assets,
                &row(7, "a", ManagedComponentArtifactKind::AssetObject, 0),
            )
            .unwrap();
        assert_eq!(
            ordinals.push(
                ManagedComponentKind::Assets,
                &row(7, "b", ManagedComponentArtifactKind::AssetObject, 0,),
            ),
            Err(ComponentTableError),
        );
    }

    #[test]
    fn incremental_builder_and_stream_parser_bind_shard_order_and_hashes() {
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Assets, 257, [0x11; 16], [0x22; 32])
                .unwrap();
        let (first, first_descriptor) = builder.push_shard(asset_rows(0, 256)).unwrap();
        assert_eq!(first_descriptor.shard_index, 0);
        assert_eq!(first_descriptor.first_row, 0);
        assert_eq!(first_descriptor.row_count, 256);
        assert_eq!(first_descriptor.byte_len as usize, first.len());
        assert_eq!(
            first_descriptor.sha256,
            <[u8; 32]>::from(Sha256::digest(&first))
        );
        let (second, second_descriptor) = builder.push_shard(asset_rows(256, 257)).unwrap();
        assert_eq!(second_descriptor.shard_index, 1);
        assert_eq!(second_descriptor.first_row, 256);
        assert_eq!(second_descriptor.row_count, 1);
        assert_eq!(second_descriptor.byte_len as usize, second.len());
        assert_eq!(
            second_descriptor.sha256,
            <[u8; 32]>::from(Sha256::digest(&second))
        );
        let (manifest, expected_summary) = builder.finish().unwrap();
        assert_eq!(manifest.shards, vec![first_descriptor, second_descriptor]);
        let encoded = [first, second];
        let intent_bytes = encode_component_intent_manifest(&manifest).unwrap();
        assert_eq!(intent_bytes.len(), 160 + 2 * 64);
        assert_eq!(
            decode_component_intent_manifest(&intent_bytes).unwrap(),
            manifest
        );

        let mut parser = ComponentTableParser::new(manifest.clone()).unwrap();
        assert_eq!(parser.parse_next(&encoded[1]), Err(ComponentTableError));
        let mut parser = ComponentTableParser::new(manifest).unwrap();
        parser.parse_next(&encoded[0]).unwrap();
        parser.parse_next(&encoded[1]).unwrap();
        assert_eq!(parser.finish().unwrap(), expected_summary);
    }

    #[test]
    fn incremental_builder_enforces_exact_boundaries_and_poisoned_global_order() {
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Assets, 257, [0x11; 16], [0x22; 32])
                .unwrap();
        assert_eq!(
            builder.push_shard(asset_rows(0, 255)),
            Err(ComponentTableError)
        );
        builder.push_shard(asset_rows(0, 256)).unwrap();
        builder.push_shard(asset_rows(256, 257)).unwrap();
        assert_eq!(builder.push_shard(Vec::new()), Err(ComponentTableError));
        builder.finish().expect("complete builder remains valid");

        let mut incomplete =
            ComponentTableBuilder::new(ManagedComponentKind::Assets, 257, [0x11; 16], [0x22; 32])
                .unwrap();
        incomplete.push_shard(asset_rows(0, 256)).unwrap();
        assert_eq!(incomplete.finish(), Err(ComponentTableError));

        let mut out_of_order =
            ComponentTableBuilder::new(ManagedComponentKind::Assets, 257, [0x11; 16], [0x22; 32])
                .unwrap();
        out_of_order.push_shard(asset_rows(0, 256)).unwrap();
        assert_eq!(
            out_of_order.push_shard(vec![row(
                256,
                "a-before-objects",
                ManagedComponentArtifactKind::AssetObject,
                1,
            )]),
            Err(ComponentTableError)
        );
        assert_eq!(
            out_of_order.push_shard(asset_rows(256, 257)),
            Err(ComponentTableError)
        );
        assert_eq!(out_of_order.finish(), Err(ComponentTableError));
    }

    #[test]
    fn binary_intent_rejects_truncation_overflow_reserved_and_digest_drift() {
        let table = encoded_three_row_table();
        let (manifest, _) = build_component_intent_manifest(
            ManagedComponentKind::Libraries,
            [0x11; 16],
            [0x22; 32],
            std::slice::from_ref(&table),
        )
        .unwrap();
        let bytes = encode_component_intent_manifest(&manifest).unwrap();
        for length in 0..bytes.len() {
            assert_eq!(
                decode_component_intent_manifest(&bytes[..length]),
                Err(ComponentTableError),
                "accepted intent truncation at {length}",
            );
        }
        for offset in [11, 22, 156] {
            let mut corrupted = bytes.clone();
            corrupted[offset] = 1;
            assert_eq!(
                decode_component_intent_manifest(&corrupted),
                Err(ComponentTableError),
                "accepted intent reserved byte at {offset}",
            );
        }
        let mut descriptor_overflow = bytes.clone();
        descriptor_overflow[160 + 12..160 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_component_intent_manifest(&descriptor_overflow),
            Err(ComponentTableError),
        );

        let mut wrong_logical = manifest;
        wrong_logical.logical_rows_sha256[0] ^= 1;
        let mut parser = ComponentTableParser::new(wrong_logical).unwrap();
        parser.parse_next(&table).unwrap();
        assert_eq!(parser.finish(), Err(ComponentTableError));
    }

    #[test]
    fn logical_and_projection_hashes_have_golden_domain_separated_encodings() {
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Libraries, 1, [0x11; 16], [0x22; 32])
                .unwrap();
        builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 7,
                final_size: 11,
                final_sha1: [0x11; 20],
                kind: ManagedComponentArtifactKind::Library,
                path: path("a.jar"),
                first_created_depth: None,
                prior: None,
            }])
            .unwrap();
        let (manifest, _) = builder.finish().unwrap();
        assert_eq!(
            digest_hex(manifest.logical_rows_sha256),
            "9c8263a8cf4be4cf2d32bb67a7bdfeadfc435e6d388378ecd70dc11268caead0"
        );
        assert_eq!(
            digest_hex(manifest.projection_sha256),
            "5117325c8404f83016626f05d74f51a00fe1d47b20ca0a473506d353aad1dfc8"
        );
    }
}
