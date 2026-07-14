use crate::artifact_path::ArtifactRelativePath;
use crate::managed_component_table::{
    ComponentCreatedAncestor, ComponentTableError, MAX_COMPONENT_PATH_BYTES,
    MAX_CREATED_ANCESTOR_PATH_BYTES, MAX_CREATED_ANCESTORS, ManagedComponentKind,
    decode_component_intent_manifest,
};
use crate::managed_fs::ManagedDirectoryIdentity;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;

pub(crate) const COMPONENT_ANCESTOR_RECORDS_PER_SHARD: usize = 256;
pub(crate) const COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES: usize = 160;
const COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES: usize = 44;
const COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES: usize = 32;
pub(crate) const MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES: usize =
    COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES
        + COMPONENT_ANCESTOR_RECORDS_PER_SHARD
            * (COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES + MAX_COMPONENT_PATH_BYTES)
        + COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES;

const JOURNAL_MAGIC: &[u8; 8] = b"AXCPANC\0";
const FORMAT_VERSION: u16 = 1;
const COMPONENT_ROOT_TARGET: u8 = 1;
const RELATIVE_TARGET: u8 = 2;
const CHECKSUM_DOMAIN: &[u8] = b"axial.component.ancestor-journal.shard.v1\0";
const TARGET_LIST_DOMAIN: &[u8] = b"axial.component.ancestor-journal.targets.v1\0";
const PORTABLE_TARGET_DOMAIN: &[u8] = b"axial.component.ancestor-journal.portable-target.v1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ComponentAncestorJournalBinding {
    component: ManagedComponentKind,
    shard_count: u32,
    total_records: u32,
    total_path_bytes: u32,
    transaction_nonce: [u8; 16],
    root_binding_sha256: [u8; 32],
    intent_sha256: [u8; 32],
    target_list_sha256: [u8; 32],
}

pub(crate) struct ComponentAncestorJournalAuthority<'a> {
    binding: ComponentAncestorJournalBinding,
    targets: &'a [ComponentCreatedAncestor],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentAncestorJournalRecord {
    ordinal: u32,
    target: ComponentCreatedAncestor,
    directory_identity_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentAncestorJournalShard {
    binding: ComponentAncestorJournalBinding,
    shard_index: u32,
    records: Vec<ComponentAncestorJournalRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed component ancestor journal is invalid")]
pub(crate) struct ComponentAncestorJournalError;

impl From<ComponentTableError> for ComponentAncestorJournalError {
    fn from(_: ComponentTableError) -> Self {
        Self
    }
}

impl<'a> ComponentAncestorJournalAuthority<'a> {
    pub(crate) fn new(
        encoded_intent: &[u8],
        targets: &'a [ComponentCreatedAncestor],
    ) -> Result<Self, ComponentAncestorJournalError> {
        let intent = decode_component_intent_manifest(encoded_intent)?;
        let total_records =
            u32::try_from(targets.len()).map_err(|_| ComponentAncestorJournalError)?;
        let (total_path_bytes, target_list_sha256) = validate_targets(targets, total_records)?;
        let shard_count = u32::try_from(expected_ancestor_journal_shard_count(targets.len())?)
            .map_err(|_| ComponentAncestorJournalError)?;
        Ok(Self {
            binding: ComponentAncestorJournalBinding {
                component: intent.component,
                shard_count,
                total_records,
                total_path_bytes: u32::try_from(total_path_bytes)
                    .map_err(|_| ComponentAncestorJournalError)?,
                transaction_nonce: intent.transaction_nonce,
                root_binding_sha256: intent.root_binding_sha256,
                intent_sha256: Sha256::digest(encoded_intent).into(),
                target_list_sha256,
            },
            targets,
        })
    }

    pub(crate) fn component(&self) -> ManagedComponentKind {
        self.binding.component
    }

    pub(crate) fn shard_count(&self) -> usize {
        self.binding.shard_count as usize
    }

    pub(crate) fn total_records(&self) -> usize {
        self.binding.total_records as usize
    }

    pub(crate) fn create_shard(
        &self,
        shard_index: usize,
        records: Vec<ComponentAncestorJournalRecord>,
    ) -> Result<ComponentAncestorJournalShard, ComponentAncestorJournalError> {
        let shard = ComponentAncestorJournalShard {
            binding: self.binding.clone(),
            shard_index: u32::try_from(shard_index).map_err(|_| ComponentAncestorJournalError)?,
            records,
        };
        validate_shard(&shard, self.targets)?;
        Ok(shard)
    }

    pub(crate) fn encode_shard(
        &self,
        shard: &ComponentAncestorJournalShard,
    ) -> Result<Vec<u8>, ComponentAncestorJournalError> {
        if shard.binding != self.binding {
            return Err(ComponentAncestorJournalError);
        }
        validate_shard(shard, self.targets)?;
        encode_component_ancestor_journal_shard(shard)
    }

    pub(crate) fn decode_shard(
        &self,
        bytes: &[u8],
    ) -> Result<ComponentAncestorJournalShard, ComponentAncestorJournalError> {
        decode_component_ancestor_journal_shard(bytes, &self.binding, self.targets)
    }
}

impl ComponentAncestorJournalRecord {
    pub(crate) fn new(
        ordinal: usize,
        target: ComponentCreatedAncestor,
        identity: ManagedDirectoryIdentity,
    ) -> Result<Self, ComponentAncestorJournalError> {
        validate_target(&target)?;
        Ok(Self {
            ordinal: u32::try_from(ordinal).map_err(|_| ComponentAncestorJournalError)?,
            target,
            directory_identity_sha256: persistent_identity_sha256(identity),
        })
    }

    pub(crate) fn ordinal(&self) -> usize {
        self.ordinal as usize
    }

    pub(crate) fn target(&self) -> &ComponentCreatedAncestor {
        &self.target
    }

    pub(crate) fn matches_identity(&self, identity: ManagedDirectoryIdentity) -> bool {
        self.directory_identity_sha256 == persistent_identity_sha256(identity)
    }
}

impl ComponentAncestorJournalShard {
    pub(crate) fn shard_index(&self) -> usize {
        self.shard_index as usize
    }

    pub(crate) fn records(&self) -> &[ComponentAncestorJournalRecord] {
        &self.records
    }
}

pub(crate) fn expected_ancestor_journal_shard_count(
    total_records: usize,
) -> Result<usize, ComponentAncestorJournalError> {
    if total_records > MAX_CREATED_ANCESTORS {
        return Err(ComponentAncestorJournalError);
    }
    total_records
        .checked_add(COMPONENT_ANCESTOR_RECORDS_PER_SHARD - 1)
        .map(|records| records / COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .ok_or(ComponentAncestorJournalError)
}

fn expected_shard_record_count(
    total_records: usize,
    shard_index: usize,
) -> Result<usize, ComponentAncestorJournalError> {
    let shard_count = expected_ancestor_journal_shard_count(total_records)?;
    if shard_index >= shard_count {
        return Err(ComponentAncestorJournalError);
    }
    let first_ordinal = shard_index
        .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .ok_or(ComponentAncestorJournalError)?;
    Ok((total_records - first_ordinal).min(COMPONENT_ANCESTOR_RECORDS_PER_SHARD))
}

fn encode_component_ancestor_journal_shard(
    shard: &ComponentAncestorJournalShard,
) -> Result<Vec<u8>, ComponentAncestorJournalError> {
    validate_shard_geometry(shard)?;
    let mut records_len = 0_usize;
    let mut shard_path_bytes = 0_usize;
    for record in &shard.records {
        records_len = records_len
            .checked_add(encoded_record_len(record)?)
            .ok_or(ComponentAncestorJournalError)?;
        shard_path_bytes = shard_path_bytes
            .checked_add(target_path_len(&record.target))
            .filter(|bytes| *bytes <= MAX_CREATED_ANCESTOR_PATH_BYTES)
            .ok_or(ComponentAncestorJournalError)?;
    }
    let body_len = COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES
        .checked_add(records_len)
        .ok_or(ComponentAncestorJournalError)?;
    let total_len = body_len
        .checked_add(COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES)
        .filter(|length| *length <= MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES)
        .ok_or(ComponentAncestorJournalError)?;
    let first_ordinal = (shard.shard_index as usize)
        .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .ok_or(ComponentAncestorJournalError)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_len)
        .map_err(|_| ComponentAncestorJournalError)?;
    bytes.extend_from_slice(JOURNAL_MAGIC);
    put_u16(&mut bytes, FORMAT_VERSION);
    bytes.push(shard.binding.component as u8);
    bytes.push(0);
    put_u32(&mut bytes, shard.shard_index);
    put_u32(&mut bytes, shard.binding.shard_count);
    put_u32(
        &mut bytes,
        u32::try_from(first_ordinal).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u32(
        &mut bytes,
        u32::try_from(shard.records.len()).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u32(&mut bytes, shard.binding.total_records);
    put_u32(
        &mut bytes,
        u32::try_from(records_len).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u32(&mut bytes, shard.binding.total_path_bytes);
    put_u32(
        &mut bytes,
        u32::try_from(shard_path_bytes).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u32(&mut bytes, 0);
    bytes.extend_from_slice(&shard.binding.transaction_nonce);
    bytes.extend_from_slice(&shard.binding.root_binding_sha256);
    bytes.extend_from_slice(&shard.binding.intent_sha256);
    bytes.extend_from_slice(&shard.binding.target_list_sha256);
    if bytes.len() != COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES {
        return Err(ComponentAncestorJournalError);
    }
    for record in &shard.records {
        encode_record(record, &mut bytes)?;
    }
    if bytes.len() != body_len {
        return Err(ComponentAncestorJournalError);
    }
    let checksum = journal_checksum(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

fn decode_component_ancestor_journal_shard(
    bytes: &[u8],
    expected_binding: &ComponentAncestorJournalBinding,
    expected_targets: &[ComponentCreatedAncestor],
) -> Result<ComponentAncestorJournalShard, ComponentAncestorJournalError> {
    if bytes.len()
        < COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES + COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES
        || bytes.len() > MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES
    {
        return Err(ComponentAncestorJournalError);
    }
    let body_len = bytes.len() - COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES;
    let (body, encoded_checksum) = bytes.split_at(body_len);
    if encoded_checksum != journal_checksum(body).as_slice() {
        return Err(ComponentAncestorJournalError);
    }
    let mut cursor = ByteCursor::new(body);
    cursor.expect(JOURNAL_MAGIC)?;
    if cursor.u16()? != FORMAT_VERSION {
        return Err(ComponentAncestorJournalError);
    }
    let component = ManagedComponentKind::from_byte(cursor.u8()?)?;
    if cursor.u8()? != 0 {
        return Err(ComponentAncestorJournalError);
    }
    let shard_index = cursor.u32()?;
    let shard_count = cursor.u32()?;
    let first_ordinal = cursor.u32()?;
    let record_count = usize::try_from(cursor.u32()?).map_err(|_| ComponentAncestorJournalError)?;
    let total_records = cursor.u32()?;
    let records_len = usize::try_from(cursor.u32()?).map_err(|_| ComponentAncestorJournalError)?;
    let total_path_bytes = cursor.u32()?;
    let encoded_shard_path_bytes =
        usize::try_from(cursor.u32()?).map_err(|_| ComponentAncestorJournalError)?;
    if cursor.u32()? != 0 {
        return Err(ComponentAncestorJournalError);
    }
    let binding = ComponentAncestorJournalBinding {
        component,
        shard_count,
        total_records,
        total_path_bytes,
        transaction_nonce: cursor.array()?,
        root_binding_sha256: cursor.array()?,
        intent_sha256: cursor.array()?,
        target_list_sha256: cursor.array()?,
    };
    if cursor.position() != COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES
        || binding != *expected_binding
        || record_count > COMPONENT_ANCESTOR_RECORDS_PER_SHARD
        || records_len != body.len() - COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES
    {
        return Err(ComponentAncestorJournalError);
    }
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(|_| ComponentAncestorJournalError)?;
    let mut shard_path_bytes = 0_usize;
    for _ in 0..record_count {
        let record = decode_record(&mut cursor)?;
        shard_path_bytes = shard_path_bytes
            .checked_add(target_path_len(&record.target))
            .filter(|bytes| *bytes <= MAX_CREATED_ANCESTOR_PATH_BYTES)
            .ok_or(ComponentAncestorJournalError)?;
        records.push(record);
    }
    if !cursor.finished() || shard_path_bytes != encoded_shard_path_bytes {
        return Err(ComponentAncestorJournalError);
    }
    let shard = ComponentAncestorJournalShard {
        binding,
        shard_index,
        records,
    };
    validate_shard(&shard, expected_targets)?;
    let expected_first = (shard.shard_index as usize)
        .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ComponentAncestorJournalError)?;
    if first_ordinal != expected_first || encode_component_ancestor_journal_shard(&shard)? != bytes
    {
        return Err(ComponentAncestorJournalError);
    }
    Ok(shard)
}

fn validate_targets(
    targets: &[ComponentCreatedAncestor],
    total_records: u32,
) -> Result<(usize, [u8; 32]), ComponentAncestorJournalError> {
    if targets.len() > MAX_CREATED_ANCESTORS {
        return Err(ComponentAncestorJournalError);
    }
    let mut path_bytes = 0_usize;
    let mut previous = None;
    let mut portable_targets = HashSet::new();
    portable_targets
        .try_reserve(targets.len())
        .map_err(|_| ComponentAncestorJournalError)?;
    let mut exact_targets = Sha256::new();
    exact_targets.update(TARGET_LIST_DOMAIN);
    exact_targets.update(total_records.to_le_bytes());
    for (ordinal, target) in targets.iter().enumerate() {
        validate_target(target)?;
        if (matches!(target, ComponentCreatedAncestor::ComponentRoot) && ordinal != 0)
            || previous.is_some_and(|previous| previous >= target)
        {
            return Err(ComponentAncestorJournalError);
        }
        if !portable_targets.insert(portable_target_sha256(target)?) {
            return Err(ComponentAncestorJournalError);
        }
        let path = target_path_bytes(target);
        exact_targets.update(
            u32::try_from(ordinal)
                .map_err(|_| ComponentAncestorJournalError)?
                .to_le_bytes(),
        );
        exact_targets.update([target_kind(target)]);
        exact_targets.update(
            u16::try_from(path.len())
                .map_err(|_| ComponentAncestorJournalError)?
                .to_le_bytes(),
        );
        exact_targets.update(path);
        path_bytes = path_bytes
            .checked_add(target_path_len(target))
            .filter(|bytes| *bytes <= MAX_CREATED_ANCESTOR_PATH_BYTES)
            .ok_or(ComponentAncestorJournalError)?;
        previous = Some(target);
    }
    Ok((path_bytes, exact_targets.finalize().into()))
}

fn validate_target(target: &ComponentCreatedAncestor) -> Result<(), ComponentAncestorJournalError> {
    if let ComponentCreatedAncestor::Relative(path) = target {
        if path.as_str().is_empty() || path.as_str().len() > MAX_COMPONENT_PATH_BYTES {
            return Err(ComponentAncestorJournalError);
        }
        path.portable_persisted_key()
            .map_err(|_| ComponentAncestorJournalError)?;
    }
    Ok(())
}

fn validate_shard(
    shard: &ComponentAncestorJournalShard,
    expected_targets: &[ComponentCreatedAncestor],
) -> Result<(), ComponentAncestorJournalError> {
    if expected_targets.len() != shard.binding.total_records as usize {
        return Err(ComponentAncestorJournalError);
    }
    validate_shard_geometry(shard)?;
    let first = (shard.shard_index as usize)
        .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .ok_or(ComponentAncestorJournalError)?;
    let end = first
        .checked_add(shard.records.len())
        .ok_or(ComponentAncestorJournalError)?;
    let expected = expected_targets
        .get(first..end)
        .ok_or(ComponentAncestorJournalError)?;
    for (offset, (record, target)) in shard.records.iter().zip(expected).enumerate() {
        if record.ordinal as usize != first + offset || record.target != *target {
            return Err(ComponentAncestorJournalError);
        }
        validate_target(&record.target)?;
    }
    Ok(())
}

fn validate_shard_geometry(
    shard: &ComponentAncestorJournalShard,
) -> Result<(), ComponentAncestorJournalError> {
    let total_records = shard.binding.total_records as usize;
    let expected_shards = expected_ancestor_journal_shard_count(total_records)?;
    let shard_index = shard.shard_index as usize;
    if total_records == 0
        || shard.binding.shard_count as usize != expected_shards
        || shard.binding.total_path_bytes as usize > MAX_CREATED_ANCESTOR_PATH_BYTES
        || shard_index >= expected_shards
        || shard.records.len() != expected_shard_record_count(total_records, shard_index)?
    {
        return Err(ComponentAncestorJournalError);
    }
    Ok(())
}

fn encode_record(
    record: &ComponentAncestorJournalRecord,
    output: &mut Vec<u8>,
) -> Result<(), ComponentAncestorJournalError> {
    let record_len = encoded_record_len(record)?;
    let path = target_path_bytes(&record.target);
    put_u16(
        output,
        u16::try_from(record_len).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u16(
        output,
        u16::try_from(path.len()).map_err(|_| ComponentAncestorJournalError)?,
    );
    put_u32(output, record.ordinal);
    output.push(target_kind(&record.target));
    output.push(0);
    put_u16(output, 0);
    output.extend_from_slice(&record.directory_identity_sha256);
    output.extend_from_slice(path);
    Ok(())
}

fn decode_record(
    cursor: &mut ByteCursor<'_>,
) -> Result<ComponentAncestorJournalRecord, ComponentAncestorJournalError> {
    let start = cursor.position();
    let record_len = usize::from(cursor.u16()?);
    let path_len = usize::from(cursor.u16()?);
    if record_len < COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES
        || path_len > MAX_COMPONENT_PATH_BYTES
        || record_len
            != COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES
                .checked_add(path_len)
                .ok_or(ComponentAncestorJournalError)?
        || start
            .checked_add(record_len)
            .is_none_or(|end| end > cursor.bytes.len())
    {
        return Err(ComponentAncestorJournalError);
    }
    let ordinal = cursor.u32()?;
    let target_kind = cursor.u8()?;
    if cursor.u8()? != 0 || cursor.u16()? != 0 {
        return Err(ComponentAncestorJournalError);
    }
    let directory_identity_sha256 = cursor.array()?;
    let path_bytes = cursor.take(path_len)?;
    let target = match target_kind {
        COMPONENT_ROOT_TARGET if path_bytes.is_empty() => ComponentCreatedAncestor::ComponentRoot,
        RELATIVE_TARGET if !path_bytes.is_empty() => {
            let path_text =
                std::str::from_utf8(path_bytes).map_err(|_| ComponentAncestorJournalError)?;
            let path =
                ArtifactRelativePath::new(path_text).map_err(|_| ComponentAncestorJournalError)?;
            if path.as_str().as_bytes() != path_bytes {
                return Err(ComponentAncestorJournalError);
            }
            ComponentCreatedAncestor::Relative(path)
        }
        _ => return Err(ComponentAncestorJournalError),
    };
    validate_target(&target)?;
    if cursor.position() != start + record_len {
        return Err(ComponentAncestorJournalError);
    }
    Ok(ComponentAncestorJournalRecord {
        ordinal,
        target,
        directory_identity_sha256,
    })
}

fn encoded_record_len(
    record: &ComponentAncestorJournalRecord,
) -> Result<usize, ComponentAncestorJournalError> {
    validate_target(&record.target)?;
    COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES
        .checked_add(target_path_len(&record.target))
        .filter(|length| *length <= u16::MAX as usize)
        .ok_or(ComponentAncestorJournalError)
}

fn target_path_len(target: &ComponentCreatedAncestor) -> usize {
    target_path_bytes(target).len()
}

fn persistent_identity_sha256(identity: ManagedDirectoryIdentity) -> [u8; 32] {
    Sha256::digest(identity.persistent_binding().as_bytes()).into()
}

fn portable_target_sha256(
    target: &ComponentCreatedAncestor,
) -> Result<[u8; 32], ComponentAncestorJournalError> {
    let portable_path = match target {
        ComponentCreatedAncestor::ComponentRoot => None,
        ComponentCreatedAncestor::Relative(path) => Some(
            path.portable_persisted_key()
                .map_err(|_| ComponentAncestorJournalError)?,
        ),
    };
    let path = portable_path.as_deref().unwrap_or("").as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(PORTABLE_TARGET_DOMAIN);
    hasher.update([target_kind(target)]);
    hasher.update(
        u16::try_from(path.len())
            .map_err(|_| ComponentAncestorJournalError)?
            .to_le_bytes(),
    );
    hasher.update(path);
    Ok(hasher.finalize().into())
}

fn target_kind(target: &ComponentCreatedAncestor) -> u8 {
    match target {
        ComponentCreatedAncestor::ComponentRoot => COMPONENT_ROOT_TARGET,
        ComponentCreatedAncestor::Relative(_) => RELATIVE_TARGET,
    }
}

fn target_path_bytes(target: &ComponentCreatedAncestor) -> &[u8] {
    match target {
        ComponentCreatedAncestor::ComponentRoot => &[],
        ComponentCreatedAncestor::Relative(path) => path.as_str().as_bytes(),
    }
}

fn journal_checksum(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHECKSUM_DOMAIN);
    hasher.update(body);
    hasher.finalize().into()
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
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

    fn take(&mut self, length: usize) -> Result<&'a [u8], ComponentAncestorJournalError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(ComponentAncestorJournalError)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn expect(&mut self, expected: &[u8]) -> Result<(), ComponentAncestorJournalError> {
        if self.take(expected.len())? != expected {
            return Err(ComponentAncestorJournalError);
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ComponentAncestorJournalError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ComponentAncestorJournalError)
    }

    fn u8(&mut self) -> Result<u8, ComponentAncestorJournalError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, ComponentAncestorJournalError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ComponentAncestorJournalError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn finished(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_component_table::{
        COMPONENT_TABLE_HEADER_BYTES, ComponentIntentManifest, ComponentShardDescriptor,
        encode_component_intent_manifest,
    };
    use crate::managed_fs::ManagedDir;
    use std::fs;

    fn path(value: &str) -> ArtifactRelativePath {
        ArtifactRelativePath::new(value).expect("test ancestor path")
    }

    fn targets() -> Vec<ComponentCreatedAncestor> {
        vec![
            ComponentCreatedAncestor::ComponentRoot,
            ComponentCreatedAncestor::Relative(path("a")),
            ComponentCreatedAncestor::Relative(path("a/b")),
        ]
    }

    fn encoded_intent() -> Vec<u8> {
        encode_component_intent_manifest(&ComponentIntentManifest {
            component: ManagedComponentKind::Libraries,
            total_rows: 1,
            final_bytes: 0,
            prior_bytes: 0,
            transaction_nonce: [0x11; 16],
            root_binding_sha256: [0x22; 32],
            logical_rows_sha256: [0x33; 32],
            projection_sha256: [0x44; 32],
            shards: vec![ComponentShardDescriptor {
                shard_index: 0,
                first_row: 0,
                row_count: 1,
                byte_len: COMPONENT_TABLE_HEADER_BYTES as u32,
                final_bytes: 0,
                prior_bytes: 0,
                sha256: [0x55; 32],
            }],
        })
        .expect("test intent")
    }

    fn recompute_checksum(bytes: &mut [u8]) {
        let body_len = bytes.len() - COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES;
        let checksum = journal_checksum(&bytes[..body_len]);
        bytes[body_len..].copy_from_slice(&checksum);
    }

    fn encoded_shard(
        authority: &ComponentAncestorJournalAuthority<'_>,
        root: &ManagedDir,
        shard_index: usize,
    ) -> Vec<u8> {
        let first = shard_index * COMPONENT_ANCESTOR_RECORDS_PER_SHARD;
        let count = expected_shard_record_count(authority.total_records(), shard_index).unwrap();
        let identity = root.identity().unwrap();
        let records = authority.targets[first..first + count]
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, target)| {
                ComponentAncestorJournalRecord::new(first + offset, target, identity).unwrap()
            })
            .collect();
        let shard = authority.create_shard(shard_index, records).unwrap();
        authority.encode_shard(&shard).unwrap()
    }

    #[test]
    fn zero_one_and_maximum_geometry_is_exact() {
        assert_eq!(expected_ancestor_journal_shard_count(0).unwrap(), 0);
        assert_eq!(expected_ancestor_journal_shard_count(1).unwrap(), 1);
        assert_eq!(expected_shard_record_count(1, 0).unwrap(), 1);
        assert_eq!(
            expected_ancestor_journal_shard_count(MAX_CREATED_ANCESTORS).unwrap(),
            MAX_CREATED_ANCESTORS.div_ceil(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        );
        let last = expected_ancestor_journal_shard_count(MAX_CREATED_ANCESTORS).unwrap() - 1;
        assert_eq!(
            expected_shard_record_count(MAX_CREATED_ANCESTORS, last).unwrap(),
            MAX_CREATED_ANCESTORS - last * COMPONENT_ANCESTOR_RECORDS_PER_SHARD
        );
        assert_eq!(
            expected_ancestor_journal_shard_count(MAX_CREATED_ANCESTORS + 1),
            Err(ComponentAncestorJournalError)
        );
    }

    #[test]
    fn journal_roundtrip_binds_exact_header_targets_ordinals_and_identities() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let expected_targets = targets();
        let encoded_intent = encoded_intent();
        let authority =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &expected_targets).unwrap();
        let encoded = encoded_shard(&authority, &root, 0);

        let decoded = authority.decode_shard(&encoded).unwrap();

        assert_eq!(authority.component(), ManagedComponentKind::Libraries);
        assert_eq!(authority.shard_count(), 1);
        assert_eq!(authority.total_records(), 3);
        assert_eq!(decoded.shard_index(), 0);
        assert_eq!(decoded.records().len(), 3);
        for (ordinal, record) in decoded.records().iter().enumerate() {
            assert_eq!(record.ordinal(), ordinal);
            assert_eq!(record.target(), &expected_targets[ordinal]);
            assert!(record.matches_identity(root.identity().unwrap()));
        }
        assert_eq!(authority.encode_shard(&decoded).unwrap(), encoded);
    }

    #[test]
    fn live_identity_replacement_does_not_match_the_durable_record() {
        let temporary = tempfile::tempdir().unwrap();
        let created = temporary.path().join("created");
        fs::create_dir(&created).unwrap();
        let admitted = ManagedDir::open_root(&created).unwrap();
        let record = ComponentAncestorJournalRecord::new(
            0,
            ComponentCreatedAncestor::ComponentRoot,
            admitted.identity().unwrap(),
        )
        .unwrap();
        drop(admitted);
        fs::rename(&created, temporary.path().join("saved")).unwrap();
        fs::create_dir(&created).unwrap();
        let replacement = ManagedDir::open_root(&created).unwrap();

        assert!(!record.matches_identity(replacement.identity().unwrap()));
    }

    #[test]
    fn decode_rejects_path_header_ordinal_reserved_and_geometry_drift() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let expected_targets = targets();
        let encoded_intent = encoded_intent();
        let authority =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &expected_targets).unwrap();
        let encoded = encoded_shard(&authority, &root, 0);

        let first_record = COMPONENT_ANCESTOR_JOURNAL_HEADER_BYTES;
        let second_record = first_record + COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES;
        let third_record = second_record + COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES + 1;
        let cases = [
            (11, 1_u8, "header reserved"),
            (44, 1, "header reserved word"),
            (64, 0x99, "root binding"),
            (128, 0x99, "target-list digest"),
            (12, 1, "shard geometry"),
            (second_record + 4, 9, "ordinal"),
            (second_record + 9, 1, "record reserved"),
            (
                third_record + COMPONENT_ANCESTOR_JOURNAL_RECORD_PREFIX_BYTES,
                b'c',
                "path",
            ),
        ];
        for (offset, value, label) in cases {
            let mut drifted = encoded.clone();
            drifted[offset] = value;
            recompute_checksum(&mut drifted);
            assert_eq!(
                authority.decode_shard(&drifted),
                Err(ComponentAncestorJournalError),
                "accepted {label} drift",
            );
        }

        let mut identity_drift = encoded.clone();
        identity_drift[second_record + 12] ^= 1;
        assert_eq!(
            authority.decode_shard(&identity_drift),
            Err(ComponentAncestorJournalError)
        );
    }

    #[test]
    fn decode_rejects_every_truncation_trailing_bytes_and_checksum_without_domain() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let expected_targets = targets();
        let encoded_intent = encoded_intent();
        let authority =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &expected_targets).unwrap();
        let encoded = encoded_shard(&authority, &root, 0);
        for length in 0..encoded.len() {
            assert_eq!(
                authority.decode_shard(&encoded[..length]),
                Err(ComponentAncestorJournalError),
                "accepted truncation at {length}",
            );
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            authority.decode_shard(&trailing),
            Err(ComponentAncestorJournalError)
        );
        let body_len = encoded.len() - COMPONENT_ANCESTOR_JOURNAL_CHECKSUM_BYTES;
        assert_ne!(
            Sha256::digest(&encoded[..body_len]).as_slice(),
            &encoded[body_len..]
        );
    }

    #[test]
    fn authority_rejects_portable_alias_order_and_wrong_shard_slice() {
        let encoded_intent = encoded_intent();
        let aliases = vec![
            ComponentCreatedAncestor::Relative(path("A")),
            ComponentCreatedAncestor::Relative(path("a")),
        ];
        assert!(matches!(
            ComponentAncestorJournalAuthority::new(&encoded_intent, &aliases),
            Err(ComponentAncestorJournalError)
        ));
        let root_after_relative = vec![
            ComponentCreatedAncestor::Relative(path("a")),
            ComponentCreatedAncestor::ComponentRoot,
        ];
        assert!(matches!(
            ComponentAncestorJournalAuthority::new(&encoded_intent, &root_after_relative),
            Err(ComponentAncestorJournalError)
        ));

        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let expected_targets = targets();
        let authority =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &expected_targets).unwrap();
        let wrong_targets = [
            expected_targets[1].clone(),
            expected_targets[0].clone(),
            expected_targets[2].clone(),
        ];
        let wrong = wrong_targets
            .into_iter()
            .enumerate()
            .map(|(ordinal, target)| {
                ComponentAncestorJournalRecord::new(ordinal, target, root.identity().unwrap())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            authority.create_shard(0, wrong),
            Err(ComponentAncestorJournalError)
        );
    }

    #[test]
    fn authority_digest_prevents_binding_and_target_list_mixing() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let encoded_intent = encoded_intent();
        let original_targets = targets();
        let alternate_targets = vec![
            ComponentCreatedAncestor::ComponentRoot,
            ComponentCreatedAncestor::Relative(path("a")),
            ComponentCreatedAncestor::Relative(path("a/c")),
        ];
        let original =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &original_targets).unwrap();
        let alternate =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &alternate_targets).unwrap();
        let encoded = encoded_shard(&original, &root, 0);

        assert_ne!(
            original.binding.target_list_sha256,
            alternate.binding.target_list_sha256
        );
        assert_eq!(
            alternate.decode_shard(&encoded),
            Err(ComponentAncestorJournalError)
        );
    }

    #[test]
    fn authority_selects_the_exact_second_shard_target_slice() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let encoded_intent = encoded_intent();
        let expected_targets = (0..257)
            .map(|ordinal| ComponentCreatedAncestor::Relative(path(&format!("p{ordinal:03}"))))
            .collect::<Vec<_>>();
        let authority =
            ComponentAncestorJournalAuthority::new(&encoded_intent, &expected_targets).unwrap();
        let encoded = encoded_shard(&authority, &root, 1);
        let decoded = authority.decode_shard(&encoded).unwrap();

        assert_eq!(decoded.shard_index(), 1);
        assert_eq!(decoded.records().len(), 1);
        assert_eq!(decoded.records()[0].ordinal(), 256);
        assert_eq!(decoded.records()[0].target(), &expected_targets[256]);

        let wrong = vec![
            ComponentAncestorJournalRecord::new(
                256,
                expected_targets[255].clone(),
                root.identity().unwrap(),
            )
            .unwrap(),
        ];
        assert_eq!(
            authority.create_shard(1, wrong),
            Err(ComponentAncestorJournalError)
        );
    }
}
