use crate::known_good::MAX_TIER2_AGGREGATE_BYTES;
use crate::managed_component_table::{
    ComponentIntentManifest, ComponentTableError, ComponentTableRow, MAX_COMPONENT_INTENT_BYTES,
    MAX_COMPONENT_TABLE_ROWS, ManagedComponentKind, decode_component_intent_manifest,
    expected_shard_count,
};
use sha2::{Digest as _, Sha256};

pub(crate) const COMPONENT_TABLE_DIRECTORY: &str = "table";
pub(crate) const COMPONENT_STAGING_DIRECTORY: &str = "staging";
pub(crate) const COMPONENT_QUARANTINE_DIRECTORY: &str = "quarantine";
pub(crate) const COMPONENT_INTENT_FILE: &str = "intent.bin";
pub(crate) const COMPONENT_OUTCOME_FILE: &str = "outcome.bin";
pub(crate) const COMPONENT_SETTLEMENT_FILE: &str = "settlement.bin";

const COMPONENT_OUTCOME_BODY_BYTES: usize = 184;
pub(crate) const COMPONENT_OUTCOME_BYTES: usize = COMPONENT_OUTCOME_BODY_BYTES + 32;
pub(crate) const COMPONENT_SETTLEMENT_HEADER_BYTES: usize = 96;
pub(crate) const MAX_COMPONENT_SETTLEMENT_BYTES: usize =
    COMPONENT_SETTLEMENT_HEADER_BYTES + COMPONENT_OUTCOME_BYTES + MAX_COMPONENT_INTENT_BYTES;

const OUTCOME_MAGIC: &[u8; 8] = b"AXCPOUT\0";
const SETTLEMENT_MAGIC: &[u8; 8] = b"AXCPSET\0";
const FORMAT_VERSION: u16 = 1;
const OUTCOME_CHECKSUM_DOMAIN: &[u8] = b"axial.component.outcome.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ComponentTerminalOutcome {
    Committed = 1,
    RolledBack = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ComponentRollbackEffect {
    None = 0,
    Execution = 1,
    Reconciliation = 2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentOutcomeRecord {
    pub(crate) component: ManagedComponentKind,
    pub(crate) terminal: ComponentTerminalOutcome,
    pub(crate) effect: ComponentRollbackEffect,
    pub(crate) total_rows: u32,
    pub(crate) shard_count: u32,
    pub(crate) final_bytes: u64,
    pub(crate) prior_bytes: u64,
    pub(crate) transaction_nonce: [u8; 16],
    pub(crate) root_binding_sha256: [u8; 32],
    pub(crate) intent_sha256: [u8; 32],
    pub(crate) logical_rows_sha256: [u8; 32],
    pub(crate) projection_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentSettlementRecord {
    pub(crate) outcome: ComponentOutcomeRecord,
    pub(crate) intent: ComponentIntentManifest,
    pub(crate) encoded_intent: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed component publication record is invalid")]
pub(crate) struct ComponentPublicationRecordError;

impl From<ComponentTableError> for ComponentPublicationRecordError {
    fn from(_: ComponentTableError) -> Self {
        Self
    }
}

impl ComponentTerminalOutcome {
    fn from_byte(value: u8) -> Result<Self, ComponentPublicationRecordError> {
        match value {
            1 => Ok(Self::Committed),
            2 => Ok(Self::RolledBack),
            _ => Err(ComponentPublicationRecordError),
        }
    }
}

impl ComponentRollbackEffect {
    fn from_byte(value: u8) -> Result<Self, ComponentPublicationRecordError> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Execution),
            2 => Ok(Self::Reconciliation),
            _ => Err(ComponentPublicationRecordError),
        }
    }
}

pub(crate) fn component_lane_name(component: ManagedComponentKind) -> &'static str {
    match component {
        ManagedComponentKind::Libraries => "libraries",
        ManagedComponentKind::Assets => "assets",
    }
}

impl ComponentOutcomeRecord {
    pub(crate) fn for_intent(
        encoded_intent: &[u8],
        terminal: ComponentTerminalOutcome,
        effect: ComponentRollbackEffect,
    ) -> Result<Self, ComponentPublicationRecordError> {
        let intent = decode_component_intent_manifest(encoded_intent)?;
        let outcome = Self {
            component: intent.component,
            terminal,
            effect,
            total_rows: intent.total_rows,
            shard_count: u32::try_from(intent.shards.len())
                .map_err(|_| ComponentPublicationRecordError)?,
            final_bytes: intent.final_bytes,
            prior_bytes: intent.prior_bytes,
            transaction_nonce: intent.transaction_nonce,
            root_binding_sha256: intent.root_binding_sha256,
            intent_sha256: Sha256::digest(encoded_intent).into(),
            logical_rows_sha256: intent.logical_rows_sha256,
            projection_sha256: intent.projection_sha256,
        };
        validate_outcome(&outcome)?;
        Ok(outcome)
    }

    fn binds_intent(
        &self,
        intent: &ComponentIntentManifest,
        encoded_intent: &[u8],
    ) -> Result<(), ComponentPublicationRecordError> {
        if self.component != intent.component
            || self.total_rows != intent.total_rows
            || usize::try_from(self.shard_count).map_err(|_| ComponentPublicationRecordError)?
                != intent.shards.len()
            || self.final_bytes != intent.final_bytes
            || self.prior_bytes != intent.prior_bytes
            || self.transaction_nonce != intent.transaction_nonce
            || self.root_binding_sha256 != intent.root_binding_sha256
            || self.intent_sha256 != <[u8; 32]>::from(Sha256::digest(encoded_intent))
            || self.logical_rows_sha256 != intent.logical_rows_sha256
            || self.projection_sha256 != intent.projection_sha256
        {
            return Err(ComponentPublicationRecordError);
        }
        Ok(())
    }
}

fn validate_outcome(
    outcome: &ComponentOutcomeRecord,
) -> Result<(), ComponentPublicationRecordError> {
    let effect_is_valid = matches!(
        (outcome.terminal, outcome.effect),
        (
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None
        ) | (
            ComponentTerminalOutcome::RolledBack,
            ComponentRollbackEffect::Execution | ComponentRollbackEffect::Reconciliation
        )
    );
    if !effect_is_valid
        || usize::try_from(outcome.total_rows).map_err(|_| ComponentPublicationRecordError)?
            > MAX_COMPONENT_TABLE_ROWS
        || usize::try_from(outcome.shard_count).map_err(|_| ComponentPublicationRecordError)?
            != expected_shard_count(
                usize::try_from(outcome.total_rows).map_err(|_| ComponentPublicationRecordError)?,
            )?
        || outcome.final_bytes > MAX_TIER2_AGGREGATE_BYTES
        || outcome.prior_bytes > MAX_TIER2_AGGREGATE_BYTES
    {
        return Err(ComponentPublicationRecordError);
    }
    Ok(())
}

pub(crate) fn encode_component_outcome(
    outcome: &ComponentOutcomeRecord,
) -> Result<Vec<u8>, ComponentPublicationRecordError> {
    validate_outcome(outcome)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(COMPONENT_OUTCOME_BYTES)
        .map_err(|_| ComponentPublicationRecordError)?;
    bytes.extend_from_slice(OUTCOME_MAGIC);
    put_u16(&mut bytes, FORMAT_VERSION);
    bytes.push(outcome.component as u8);
    bytes.push(outcome.terminal as u8);
    bytes.push(outcome.effect as u8);
    bytes.push(0);
    put_u16(&mut bytes, 0);
    put_u32(&mut bytes, outcome.total_rows);
    put_u32(&mut bytes, outcome.shard_count);
    put_u64(&mut bytes, outcome.final_bytes);
    put_u64(&mut bytes, outcome.prior_bytes);
    bytes.extend_from_slice(&outcome.transaction_nonce);
    bytes.extend_from_slice(&outcome.root_binding_sha256);
    bytes.extend_from_slice(&outcome.intent_sha256);
    bytes.extend_from_slice(&outcome.logical_rows_sha256);
    bytes.extend_from_slice(&outcome.projection_sha256);
    if bytes.len() != COMPONENT_OUTCOME_BODY_BYTES {
        return Err(ComponentPublicationRecordError);
    }
    let checksum = outcome_checksum(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

pub(crate) fn decode_component_outcome(
    bytes: &[u8],
) -> Result<ComponentOutcomeRecord, ComponentPublicationRecordError> {
    if bytes.len() != COMPONENT_OUTCOME_BYTES {
        return Err(ComponentPublicationRecordError);
    }
    let (body, encoded_checksum) = bytes.split_at(COMPONENT_OUTCOME_BODY_BYTES);
    let expected_checksum = outcome_checksum(body);
    if encoded_checksum != expected_checksum.as_slice() {
        return Err(ComponentPublicationRecordError);
    }
    let mut cursor = ByteCursor::new(body);
    cursor.expect(OUTCOME_MAGIC)?;
    if cursor.u16()? != FORMAT_VERSION {
        return Err(ComponentPublicationRecordError);
    }
    let component = ManagedComponentKind::from_byte(cursor.u8()?)?;
    let terminal = ComponentTerminalOutcome::from_byte(cursor.u8()?)?;
    let effect = ComponentRollbackEffect::from_byte(cursor.u8()?)?;
    if cursor.u8()? != 0 || cursor.u16()? != 0 {
        return Err(ComponentPublicationRecordError);
    }
    let outcome = ComponentOutcomeRecord {
        component,
        terminal,
        effect,
        total_rows: cursor.u32()?,
        shard_count: cursor.u32()?,
        final_bytes: cursor.u64()?,
        prior_bytes: cursor.u64()?,
        transaction_nonce: cursor.array()?,
        root_binding_sha256: cursor.array()?,
        intent_sha256: cursor.array()?,
        logical_rows_sha256: cursor.array()?,
        projection_sha256: cursor.array()?,
    };
    if !cursor.finished() {
        return Err(ComponentPublicationRecordError);
    }
    validate_outcome(&outcome)?;
    if encode_component_outcome(&outcome)? != bytes {
        return Err(ComponentPublicationRecordError);
    }
    Ok(outcome)
}

pub(crate) fn encode_component_settlement(
    outcome: &ComponentOutcomeRecord,
    encoded_intent: &[u8],
) -> Result<Vec<u8>, ComponentPublicationRecordError> {
    let intent = decode_component_intent_manifest(encoded_intent)?;
    outcome.binds_intent(&intent, encoded_intent)?;
    let encoded_outcome = encode_component_outcome(outcome)?;
    let total_bytes = COMPONENT_SETTLEMENT_HEADER_BYTES
        .checked_add(encoded_outcome.len())
        .and_then(|length| length.checked_add(encoded_intent.len()))
        .filter(|length| *length <= MAX_COMPONENT_SETTLEMENT_BYTES)
        .ok_or(ComponentPublicationRecordError)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_bytes)
        .map_err(|_| ComponentPublicationRecordError)?;
    bytes.extend_from_slice(SETTLEMENT_MAGIC);
    put_u16(&mut bytes, FORMAT_VERSION);
    bytes.push(outcome.component as u8);
    bytes.push(0);
    put_u32(
        &mut bytes,
        u32::try_from(encoded_outcome.len()).map_err(|_| ComponentPublicationRecordError)?,
    );
    put_u32(
        &mut bytes,
        u32::try_from(encoded_intent.len()).map_err(|_| ComponentPublicationRecordError)?,
    );
    bytes.extend_from_slice(&<[u8; 32]>::from(Sha256::digest(&encoded_outcome)));
    bytes.extend_from_slice(&<[u8; 32]>::from(Sha256::digest(encoded_intent)));
    put_u32(
        &mut bytes,
        u32::try_from(total_bytes).map_err(|_| ComponentPublicationRecordError)?,
    );
    put_u32(&mut bytes, 0);
    put_u32(&mut bytes, 0);
    if bytes.len() != COMPONENT_SETTLEMENT_HEADER_BYTES {
        return Err(ComponentPublicationRecordError);
    }
    bytes.extend_from_slice(&encoded_outcome);
    bytes.extend_from_slice(encoded_intent);
    Ok(bytes)
}

pub(crate) fn decode_component_settlement(
    bytes: &[u8],
) -> Result<ComponentSettlementRecord, ComponentPublicationRecordError> {
    if bytes.len() < COMPONENT_SETTLEMENT_HEADER_BYTES + COMPONENT_OUTCOME_BYTES
        || bytes.len() > MAX_COMPONENT_SETTLEMENT_BYTES
    {
        return Err(ComponentPublicationRecordError);
    }
    let mut cursor = ByteCursor::new(bytes);
    cursor.expect(SETTLEMENT_MAGIC)?;
    if cursor.u16()? != FORMAT_VERSION {
        return Err(ComponentPublicationRecordError);
    }
    let component = ManagedComponentKind::from_byte(cursor.u8()?)?;
    if cursor.u8()? != 0 {
        return Err(ComponentPublicationRecordError);
    }
    let outcome_len =
        usize::try_from(cursor.u32()?).map_err(|_| ComponentPublicationRecordError)?;
    let intent_len = usize::try_from(cursor.u32()?).map_err(|_| ComponentPublicationRecordError)?;
    let outcome_sha256 = cursor.array::<32>()?;
    let intent_sha256 = cursor.array::<32>()?;
    let total_len = usize::try_from(cursor.u32()?).map_err(|_| ComponentPublicationRecordError)?;
    if cursor.u32()? != 0
        || cursor.u32()? != 0
        || cursor.position != COMPONENT_SETTLEMENT_HEADER_BYTES
        || outcome_len != COMPONENT_OUTCOME_BYTES
        || intent_len > MAX_COMPONENT_INTENT_BYTES
        || total_len != bytes.len()
        || COMPONENT_SETTLEMENT_HEADER_BYTES
            .checked_add(outcome_len)
            .and_then(|length| length.checked_add(intent_len))
            != Some(bytes.len())
    {
        return Err(ComponentPublicationRecordError);
    }
    let encoded_outcome = cursor.take(outcome_len)?;
    let encoded_intent_slice = cursor.take(intent_len)?;
    if !cursor.finished()
        || outcome_sha256 != <[u8; 32]>::from(Sha256::digest(encoded_outcome))
        || intent_sha256 != <[u8; 32]>::from(Sha256::digest(encoded_intent_slice))
    {
        return Err(ComponentPublicationRecordError);
    }
    let outcome = decode_component_outcome(encoded_outcome)?;
    if outcome.component != component {
        return Err(ComponentPublicationRecordError);
    }
    let intent = decode_component_intent_manifest(encoded_intent_slice)?;
    outcome.binds_intent(&intent, encoded_intent_slice)?;
    let mut encoded_intent = Vec::new();
    encoded_intent
        .try_reserve_exact(intent_len)
        .map_err(|_| ComponentPublicationRecordError)?;
    encoded_intent.extend_from_slice(encoded_intent_slice);
    let record = ComponentSettlementRecord {
        outcome,
        intent,
        encoded_intent,
    };
    if encode_component_settlement(&record.outcome, &record.encoded_intent)? != bytes {
        return Err(ComponentPublicationRecordError);
    }
    Ok(record)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentObservedCanonical {
    Absent,
    Source,
    Prior,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ComponentRecoveryObservation {
    pub(crate) canonical: ComponentObservedCanonical,
    pub(crate) stage_present: bool,
    pub(crate) quarantine_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComponentEntryRecoveryShape {
    commit_candidate: bool,
    rollback_reachable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentRecoveryDecision {
    Commit,
    Rollback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed component recovery state is ambiguous")]
pub(crate) struct ComponentRecoveryAmbiguous;

pub(crate) struct ComponentRecoveryPlanner {
    expected_rows: usize,
    observed_rows: usize,
    all_commit_candidates: bool,
    all_rollback_reachable: bool,
}

impl ComponentRecoveryPlanner {
    pub(crate) fn new(expected_rows: usize) -> Result<Self, ComponentRecoveryAmbiguous> {
        if expected_rows > MAX_COMPONENT_TABLE_ROWS {
            return Err(ComponentRecoveryAmbiguous);
        }
        Ok(Self {
            expected_rows,
            observed_rows: 0,
            all_commit_candidates: true,
            all_rollback_reachable: true,
        })
    }

    pub(crate) fn observe(
        &mut self,
        row: &ComponentTableRow,
        observation: ComponentRecoveryObservation,
    ) -> Result<(), ComponentRecoveryAmbiguous> {
        if self.observed_rows >= self.expected_rows {
            return Err(ComponentRecoveryAmbiguous);
        }
        let shape = classify_component_recovery_shape(row, observation)?;
        self.all_commit_candidates &= shape.commit_candidate;
        self.all_rollback_reachable &= shape.rollback_reachable;
        self.observed_rows += 1;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ComponentRecoveryDecision, ComponentRecoveryAmbiguous> {
        if self.observed_rows != self.expected_rows {
            return Err(ComponentRecoveryAmbiguous);
        }
        if self.all_commit_candidates {
            Ok(ComponentRecoveryDecision::Commit)
        } else if self.all_rollback_reachable {
            Ok(ComponentRecoveryDecision::Rollback)
        } else {
            Err(ComponentRecoveryAmbiguous)
        }
    }
}

fn classify_component_recovery_shape(
    row: &ComponentTableRow,
    observation: ComponentRecoveryObservation,
) -> Result<ComponentEntryRecoveryShape, ComponentRecoveryAmbiguous> {
    use ComponentObservedCanonical::{Absent, Other, Prior, Source};

    if observation.canonical == Other {
        return Err(ComponentRecoveryAmbiguous);
    }
    if row.prior_is_final() {
        let exact = observation.canonical == Source
            && !observation.stage_present
            && !observation.quarantine_present;
        return exact
            .then_some(ComponentEntryRecoveryShape {
                commit_candidate: true,
                rollback_reachable: true,
            })
            .ok_or(ComponentRecoveryAmbiguous);
    }
    match &row.prior {
        None => {
            if observation.quarantine_present {
                return Err(ComponentRecoveryAmbiguous);
            }
            match (observation.canonical, observation.stage_present) {
                (Source, false) => Ok(ComponentEntryRecoveryShape {
                    commit_candidate: true,
                    rollback_reachable: true,
                }),
                (Absent, true) => Ok(ComponentEntryRecoveryShape {
                    commit_candidate: false,
                    rollback_reachable: true,
                }),
                _ => Err(ComponentRecoveryAmbiguous),
            }
        }
        Some(_) => match (
            observation.canonical,
            observation.stage_present,
            observation.quarantine_present,
        ) {
            (Source, false, true) => Ok(ComponentEntryRecoveryShape {
                commit_candidate: true,
                rollback_reachable: true,
            }),
            (Absent, true, true) | (Prior, true, false) => Ok(ComponentEntryRecoveryShape {
                commit_candidate: false,
                rollback_reachable: true,
            }),
            _ => Err(ComponentRecoveryAmbiguous),
        },
    }
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

fn outcome_checksum(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(OUTCOME_CHECKSUM_DOMAIN);
    hasher.update(body);
    hasher.finalize().into()
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], ComponentPublicationRecordError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(ComponentPublicationRecordError)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn expect(&mut self, expected: &[u8]) -> Result<(), ComponentPublicationRecordError> {
        if self.take(expected.len())? != expected {
            return Err(ComponentPublicationRecordError);
        }
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ComponentPublicationRecordError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ComponentPublicationRecordError)
    }

    fn u8(&mut self) -> Result<u8, ComponentPublicationRecordError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, ComponentPublicationRecordError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ComponentPublicationRecordError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ComponentPublicationRecordError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn finished(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_path::ArtifactRelativePath;
    use crate::managed_component_table::{
        COMPONENT_TABLE_HEADER_BYTES, COMPONENT_TABLE_ROWS_PER_SHARD, ComponentPriorFile,
        ComponentShardDescriptor, ComponentTableShard, ManagedComponentArtifactKind,
        build_component_intent_manifest, encode_component_intent_manifest,
        encode_component_table_shard,
    };

    fn row(prior: Option<ComponentPriorFile>) -> ComponentTableRow {
        ComponentTableRow {
            inventory_ordinal: 0,
            final_size: 7,
            final_sha1: [0x11; 20],
            kind: ManagedComponentArtifactKind::Library,
            path: ArtifactRelativePath::new("a.jar").unwrap(),
            first_created_depth: None,
            prior,
        }
    }

    fn intent_bytes() -> Vec<u8> {
        let table = encode_component_table_shard(&ComponentTableShard {
            component: ManagedComponentKind::Libraries,
            shard_index: 0,
            shard_count: 1,
            first_row: 0,
            total_rows: 1,
            transaction_nonce: [0x22; 16],
            root_binding_sha256: [0x33; 32],
            rows: vec![row(None)],
        })
        .unwrap();
        let (manifest, _) = build_component_intent_manifest(
            ManagedComponentKind::Libraries,
            [0x22; 16],
            [0x33; 32],
            &[table],
        )
        .unwrap();
        encode_component_intent_manifest(&manifest).unwrap()
    }

    fn maximum_intent_bytes() -> Vec<u8> {
        let shard_count = expected_shard_count(MAX_COMPONENT_TABLE_ROWS).unwrap();
        let shards = (0..shard_count)
            .map(|index| {
                let first_row = index * COMPONENT_TABLE_ROWS_PER_SHARD;
                ComponentShardDescriptor {
                    shard_index: u32::try_from(index).unwrap(),
                    first_row: u32::try_from(first_row).unwrap(),
                    row_count: u32::try_from(
                        (MAX_COMPONENT_TABLE_ROWS - first_row).min(COMPONENT_TABLE_ROWS_PER_SHARD),
                    )
                    .unwrap(),
                    byte_len: u32::try_from(COMPONENT_TABLE_HEADER_BYTES).unwrap(),
                    final_bytes: 0,
                    prior_bytes: 0,
                    sha256: [u8::try_from(index % 251).unwrap(); 32],
                }
            })
            .collect();
        encode_component_intent_manifest(&ComponentIntentManifest {
            component: ManagedComponentKind::Assets,
            total_rows: u32::try_from(MAX_COMPONENT_TABLE_ROWS).unwrap(),
            final_bytes: 0,
            prior_bytes: 0,
            transaction_nonce: [0x55; 16],
            root_binding_sha256: [0x66; 32],
            logical_rows_sha256: [0x77; 32],
            projection_sha256: [0x88; 32],
            shards,
        })
        .unwrap()
    }

    fn observation(
        canonical: ComponentObservedCanonical,
        stage_present: bool,
        quarantine_present: bool,
    ) -> ComponentRecoveryObservation {
        ComponentRecoveryObservation {
            canonical,
            stage_present,
            quarantine_present,
        }
    }

    #[test]
    fn outcome_and_settlement_roundtrip_bind_the_exact_intent() {
        let intent = intent_bytes();
        let outcome = ComponentOutcomeRecord::for_intent(
            &intent,
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None,
        )
        .unwrap();
        let encoded_outcome = encode_component_outcome(&outcome).unwrap();
        assert_eq!(encoded_outcome.len(), COMPONENT_OUTCOME_BYTES);
        assert_eq!(decode_component_outcome(&encoded_outcome).unwrap(), outcome);

        let settlement = encode_component_settlement(&outcome, &intent).unwrap();
        let decoded = decode_component_settlement(&settlement).unwrap();
        assert_eq!(decoded.outcome, outcome);
        assert_eq!(decoded.encoded_intent, intent);
        assert_eq!(
            encode_component_intent_manifest(&decoded.intent).unwrap(),
            intent
        );
        assert_eq!(
            settlement.len(),
            COMPONENT_SETTLEMENT_HEADER_BYTES + COMPONENT_OUTCOME_BYTES + intent.len()
        );
    }

    #[test]
    fn outcome_rejects_invalid_terminal_effect_pairs() {
        let intent = intent_bytes();
        for (terminal, effect) in [
            (
                ComponentTerminalOutcome::Committed,
                ComponentRollbackEffect::Execution,
            ),
            (
                ComponentTerminalOutcome::Committed,
                ComponentRollbackEffect::Reconciliation,
            ),
            (
                ComponentTerminalOutcome::RolledBack,
                ComponentRollbackEffect::None,
            ),
        ] {
            assert_eq!(
                ComponentOutcomeRecord::for_intent(&intent, terminal, effect),
                Err(ComponentPublicationRecordError)
            );
        }
    }

    #[test]
    fn outcome_checksum_rejects_every_single_bit_drift() {
        let intent = intent_bytes();
        let outcome = ComponentOutcomeRecord::for_intent(
            &intent,
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None,
        )
        .unwrap();
        let original = encode_component_outcome(&outcome).unwrap();
        for offset in 0..original.len() {
            for bit in 0..u8::BITS {
                let mut corrupted = original.clone();
                corrupted[offset] ^= 1 << bit;
                assert_eq!(
                    decode_component_outcome(&corrupted),
                    Err(ComponentPublicationRecordError),
                    "accepted outcome drift at byte {offset}, bit {bit}",
                );
            }
        }
    }

    #[test]
    fn settlement_accepts_the_exact_maximum_bound_and_rejects_oversize() {
        let intent = maximum_intent_bytes();
        assert_eq!(intent.len(), MAX_COMPONENT_INTENT_BYTES);
        let outcome = ComponentOutcomeRecord::for_intent(
            &intent,
            ComponentTerminalOutcome::RolledBack,
            ComponentRollbackEffect::Reconciliation,
        )
        .unwrap();
        let settlement = encode_component_settlement(&outcome, &intent).unwrap();
        assert_eq!(settlement.len(), MAX_COMPONENT_SETTLEMENT_BYTES);
        assert_eq!(
            decode_component_settlement(&settlement)
                .unwrap()
                .encoded_intent,
            intent,
        );
        assert_eq!(
            decode_component_settlement(&vec![0; MAX_COMPONENT_SETTLEMENT_BYTES + 1]),
            Err(ComponentPublicationRecordError),
        );
    }

    #[test]
    fn record_decoders_reject_every_truncation_and_trailing_byte() {
        let intent = intent_bytes();
        let outcome = ComponentOutcomeRecord::for_intent(
            &intent,
            ComponentTerminalOutcome::RolledBack,
            ComponentRollbackEffect::Reconciliation,
        )
        .unwrap();
        let outcome_bytes = encode_component_outcome(&outcome).unwrap();
        for length in 0..outcome_bytes.len() {
            assert_eq!(
                decode_component_outcome(&outcome_bytes[..length]),
                Err(ComponentPublicationRecordError)
            );
        }
        let mut trailing_outcome = outcome_bytes;
        trailing_outcome.push(0);
        assert_eq!(
            decode_component_outcome(&trailing_outcome),
            Err(ComponentPublicationRecordError)
        );

        let settlement = encode_component_settlement(&outcome, &intent).unwrap();
        for length in 0..settlement.len() {
            assert_eq!(
                decode_component_settlement(&settlement[..length]),
                Err(ComponentPublicationRecordError),
                "accepted settlement truncation at {length}",
            );
        }
        let mut trailing_settlement = settlement;
        trailing_settlement.push(0);
        assert_eq!(
            decode_component_settlement(&trailing_settlement),
            Err(ComponentPublicationRecordError)
        );
    }

    #[test]
    fn settlement_rejects_outcome_intent_or_header_drift() {
        let intent = intent_bytes();
        let outcome = ComponentOutcomeRecord::for_intent(
            &intent,
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None,
        )
        .unwrap();
        let original = encode_component_settlement(&outcome, &intent).unwrap();
        for offset in [11, 92, COMPONENT_SETTLEMENT_HEADER_BYTES + 20] {
            let mut corrupted = original.clone();
            corrupted[offset] ^= 1;
            assert_eq!(
                decode_component_settlement(&corrupted),
                Err(ComponentPublicationRecordError),
                "accepted settlement drift at {offset}",
            );
        }
    }

    #[test]
    fn new_file_truth_table_requires_the_staged_source_for_rollback() {
        let row = row(None);
        for canonical in [
            ComponentObservedCanonical::Absent,
            ComponentObservedCanonical::Source,
            ComponentObservedCanonical::Prior,
            ComponentObservedCanonical::Other,
        ] {
            for stage in [false, true] {
                for quarantine in [false, true] {
                    let result = classify_component_recovery_shape(
                        &row,
                        observation(canonical, stage, quarantine),
                    );
                    let expected = match (canonical, stage, quarantine) {
                        (ComponentObservedCanonical::Source, false, false) => Some((true, true)),
                        (ComponentObservedCanonical::Absent, true, false) => Some((false, true)),
                        _ => None,
                    };
                    assert_eq!(
                        result
                            .map(|shape| (shape.commit_candidate, shape.rollback_reachable))
                            .ok(),
                        expected,
                        "{canonical:?}/{stage}/{quarantine}",
                    );
                }
            }
        }
    }

    #[test]
    fn exact_truth_table_accepts_only_source_without_slots() {
        let exact = row(Some(ComponentPriorFile {
            size: 7,
            sha1: [0x11; 20],
        }));
        for canonical in [
            ComponentObservedCanonical::Absent,
            ComponentObservedCanonical::Source,
            ComponentObservedCanonical::Prior,
            ComponentObservedCanonical::Other,
        ] {
            for stage in [false, true] {
                for quarantine in [false, true] {
                    let result = classify_component_recovery_shape(
                        &exact,
                        observation(canonical, stage, quarantine),
                    );
                    let expected =
                        (canonical == ComponentObservedCanonical::Source && !stage && !quarantine)
                            .then_some((true, true));
                    assert_eq!(
                        result
                            .map(|shape| (shape.commit_candidate, shape.rollback_reachable))
                            .ok(),
                        expected,
                        "{canonical:?}/{stage}/{quarantine}",
                    );
                }
            }
        }
    }

    #[test]
    fn replacement_truth_table_requires_the_staged_source_for_rollback() {
        let replacement = row(Some(ComponentPriorFile {
            size: 9,
            sha1: [0x44; 20],
        }));
        for canonical in [
            ComponentObservedCanonical::Absent,
            ComponentObservedCanonical::Source,
            ComponentObservedCanonical::Prior,
            ComponentObservedCanonical::Other,
        ] {
            for stage in [false, true] {
                for quarantine in [false, true] {
                    let result = classify_component_recovery_shape(
                        &replacement,
                        observation(canonical, stage, quarantine),
                    );
                    let expected = match (canonical, stage, quarantine) {
                        (ComponentObservedCanonical::Source, false, true) => Some((true, true)),
                        (ComponentObservedCanonical::Absent, true, true)
                        | (ComponentObservedCanonical::Prior, true, false) => Some((false, true)),
                        _ => None,
                    };
                    assert_eq!(
                        result
                            .map(|shape| (shape.commit_candidate, shape.rollback_reachable))
                            .ok(),
                        expected,
                        "{canonical:?}/{stage}/{quarantine}",
                    );
                }
            }
        }
    }

    #[test]
    fn planner_requires_all_commit_candidates_or_global_rollback() {
        let absent = row(None);
        let exact = row(Some(ComponentPriorFile {
            size: 7,
            sha1: [0x11; 20],
        }));
        let replacement = row(Some(ComponentPriorFile {
            size: 9,
            sha1: [0x44; 20],
        }));
        let committed = observation(ComponentObservedCanonical::Source, false, false);
        let replacement_committed = observation(ComponentObservedCanonical::Source, false, true);

        let mut planner = ComponentRecoveryPlanner::new(3).unwrap();
        planner.observe(&absent, committed).unwrap();
        planner.observe(&exact, committed).unwrap();
        planner
            .observe(&replacement, replacement_committed)
            .unwrap();
        assert_eq!(planner.finish(), Ok(ComponentRecoveryDecision::Commit));

        let mut planner = ComponentRecoveryPlanner::new(3).unwrap();
        planner
            .observe(
                &absent,
                observation(ComponentObservedCanonical::Absent, true, false),
            )
            .unwrap();
        planner.observe(&exact, committed).unwrap();
        planner
            .observe(&replacement, replacement_committed)
            .unwrap();
        assert_eq!(planner.finish(), Ok(ComponentRecoveryDecision::Rollback));

        let mut planner = ComponentRecoveryPlanner::new(2).unwrap();
        planner.observe(&absent, committed).unwrap();
        assert_eq!(planner.finish(), Err(ComponentRecoveryAmbiguous));
    }

    #[test]
    fn component_topology_names_are_closed() {
        assert_eq!(
            component_lane_name(ManagedComponentKind::Libraries),
            "libraries"
        );
        assert_eq!(component_lane_name(ManagedComponentKind::Assets), "assets");
        assert_eq!(COMPONENT_TABLE_DIRECTORY, "table");
        assert_eq!(COMPONENT_STAGING_DIRECTORY, "staging");
        assert_eq!(COMPONENT_QUARANTINE_DIRECTORY, "quarantine");
        assert_eq!(COMPONENT_INTENT_FILE, "intent.bin");
        assert_eq!(COMPONENT_OUTCOME_FILE, "outcome.bin");
        assert_eq!(COMPONENT_SETTLEMENT_FILE, "settlement.bin");
        assert_eq!(COMPONENT_OUTCOME_BYTES, 216);
        assert_eq!(MAX_COMPONENT_SETTLEMENT_BYTES, 50_520);
    }
}
