use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, MAX_TIER2_ENTRIES};
use crate::loaders::types::LoaderError;
use crate::managed_component_ancestor_journal::COMPONENT_ANCESTOR_RECORDS_PER_SHARD;
use crate::managed_component_publication::{
    COMPONENT_INTENT_FILE, COMPONENT_QUARANTINE_DIRECTORY, COMPONENT_STAGING_DIRECTORY,
    COMPONENT_TABLE_DIRECTORY, component_lane_name,
};
use crate::managed_component_spool::{ComponentTableReplay, ComponentTableSpoolError};
use crate::managed_component_table::{
    ComponentIntentManifest, ComponentTableError, ComponentTableParser, ComponentTableRow,
    ComponentTableSummary, MAX_COMPONENT_INTENT_BYTES, MAX_COMPONENT_TABLE_SHARD_BYTES,
    MAX_COMPONENT_TABLE_SHARDS, MAX_CREATED_ANCESTORS, ManagedComponentKind, component_table_path,
    decode_component_table_shard, encode_component_intent_manifest,
};
use crate::managed_fs::{
    MAX_MANAGED_TEMP_ENTRIES, ManagedCreateOnlyWriteFailure, ManagedDir, ManagedDirectoryIdentity,
    ManagedEmptyChildRemoval, ManagedFileGuard, ManagedFileIdentity, validate_managed_temp_name,
};
use crate::managed_publication::{ManagedPublicationError, ManagedRootPublicationLease};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

mod managed_component_transaction;

#[cfg(test)]
pub(crate) use managed_component_transaction::{
    ComponentExecutionFault, ComponentSettlementFault, execute_component_intent_with_fault,
    settle_component_transaction_with_fault,
};
pub(crate) use managed_component_transaction::{
    ComponentExecutionResult, ComponentIntentPublicationRecovery, ComponentRecoveryRequired,
    ComponentRecoveryRetryResult, ComponentSettledOutcome, ComponentSettlementResult,
    ComponentStartupRecoveryResult, ComponentTransactionReceipt, execute_component_intent,
    recover_component_intent_publication, recover_component_transaction, retry_component_recovery,
    retry_component_settlement, settle_component_transaction,
};

#[cfg(test)]
use crate::managed_fs::ManagedCreateOnlyWriteFault;

const MAX_COMPONENT_LANE_ENTRIES: usize = 7;
const COMPONENT_ANCESTORS_DIRECTORY: &str = "ancestors";
const COMPONENT_ANCESTOR_RECORDS_DIRECTORY: &str = "records";
const COMPONENT_ANCESTOR_STAGING_DIRECTORY: &str = "staging";
const COMPONENT_ANCESTOR_RECORD_FILE_SUFFIX: &str = ".anc";
const MAX_COMPONENT_ANCESTOR_SHARDS: usize =
    MAX_CREATED_ANCESTORS.div_ceil(COMPONENT_ANCESTOR_RECORDS_PER_SHARD);
const COMPONENT_BUCKET_PARK_A: &str = "bucket-park-a";
const COMPONENT_BUCKET_PARK_B: &str = "bucket-park-b";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ComponentEffectsError {
    #[error("managed component filesystem topology is invalid")]
    Topology,
    #[error(transparent)]
    Filesystem(#[from] LoaderError),
    #[error(transparent)]
    Publication(#[from] ManagedPublicationError),
    #[error(transparent)]
    Table(#[from] ComponentTableError),
    #[error(transparent)]
    Spool(#[from] ComponentTableSpoolError),
}

pub(crate) struct ComponentLane {
    component: ManagedComponentKind,
    lane: ManagedDir,
    table: ManagedDir,
    staging: ManagedDir,
    quarantine: ManagedDir,
    ancestors: ManagedDir,
    ancestor_records: ManagedDir,
    ancestor_staging: ManagedDir,
}

pub(crate) struct ComponentShardBuckets {
    staging: ManagedDir,
    quarantine: ManagedDir,
}

pub(crate) struct ComponentIntentCandidate {
    lane: ComponentLane,
    lease: ManagedRootPublicationLease,
    manifest: ComponentIntentManifest,
    encoded_intent: Vec<u8>,
    summary: ComponentTableSummary,
    authority: ComponentPreintentAuthority,
}

pub(crate) struct ComponentIntentPublished {
    lane: ComponentLane,
    lease: ManagedRootPublicationLease,
    manifest: ComponentIntentManifest,
    encoded_intent: Vec<u8>,
    intent_guard: ManagedFileGuard,
}

pub(crate) enum ComponentIntentPublishFailure {
    BeforePromotion {
        candidate: Box<ComponentIntentCandidate>,
        cause: ComponentEffectsError,
    },
    PromotionAttempted {
        candidate: Box<ComponentIntentCandidate>,
        intent_guard: Option<ManagedFileGuard>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentIntentPublishFault {
    BeforeMarkerPromotion,
    PromotionAttemptedWithoutMarker,
    AfterMarkerPromotion,
    AfterLaneSynced,
    AfterPublicationSynced,
    AfterRootSynced,
    AfterLeaseRevalidated,
}

#[derive(Eq, PartialEq)]
struct ComponentPreintentAuthority {
    root: ManagedDirectoryIdentity,
    publication: ManagedDirectoryIdentity,
    lane: ManagedDirectoryIdentity,
    table: ManagedDirectoryIdentity,
    staging: ManagedDirectoryIdentity,
    quarantine: ManagedDirectoryIdentity,
    ancestors: ManagedDirectoryIdentity,
    ancestor_records: ManagedDirectoryIdentity,
    ancestor_staging: ManagedDirectoryIdentity,
    shards: Vec<ComponentPreintentShardAuthority>,
}

#[derive(Eq, PartialEq)]
struct ComponentPreintentShardAuthority {
    table: ManagedFileIdentity,
    staging: ManagedDirectoryIdentity,
    quarantine: ManagedDirectoryIdentity,
    rows: Vec<ComponentPreintentRowAuthority>,
}

#[derive(Eq, PartialEq)]
struct ComponentPreintentRowAuthority {
    staging: Option<ManagedFileIdentity>,
    canonical_anchor: ManagedDirectoryIdentity,
    canonical: Option<ManagedFileIdentity>,
}

struct ComponentPreintentCleanupPlan {
    temporary: Vec<ComponentPlannedFile>,
    table: Option<ComponentTableCleanupPlan>,
    staging: Option<ComponentBucketCleanupPlan>,
    quarantine: Option<ComponentBucketCleanupPlan>,
    ancestors: Option<ComponentAncestorsCleanupPlan>,
}

struct ComponentAncestorsCleanupPlan {
    ancestors: ManagedDir,
    ancestors_identity: ManagedDirectoryIdentity,
    records: Option<(ManagedDir, ManagedDirectoryIdentity)>,
    staging: Option<(ManagedDir, ManagedDirectoryIdentity)>,
}

struct ComponentLaneEntryPlan {
    directories: BTreeSet<String>,
    temporary: Vec<ComponentPlannedFile>,
}

struct ComponentTableCleanupPlan {
    directory: ManagedDir,
    component: ManagedComponentKind,
    files: Vec<ComponentPlannedFile>,
    temporary: Vec<ComponentPlannedFile>,
}

struct ComponentBucketCleanupPlan {
    directory: ManagedDir,
    buckets: Vec<ComponentGuardedBucket>,
    parked: Option<ComponentParkedBucket>,
}

struct ComponentGuardedBucket {
    name: String,
    identity: ManagedDirectoryIdentity,
    files: Vec<ComponentPlannedFile>,
    temporary: Vec<ComponentPlannedFile>,
}

struct ComponentParkedBucket {
    name: &'static str,
    alternate: &'static str,
    identity: ManagedDirectoryIdentity,
}

struct ComponentPlannedFile {
    name: String,
    size: u64,
    identity: ManagedFileIdentity,
}

struct ComponentDirectoryFilePlan {
    owned: Vec<ComponentPlannedFile>,
    temporary: Vec<ComponentPlannedFile>,
}

pub(crate) struct ComponentCanonicalPathPlan {
    creation_anchor: ManagedDir,
    remaining_parent_segments: Vec<String>,
    file_name: String,
    first_created_depth: Option<u16>,
}

pub(crate) enum ComponentCanonicalObservation {
    Absent,
    Regular(ComponentObservedFile),
}

pub(crate) struct ComponentObservedFile {
    parent: ManagedDir,
    file_name: String,
    guard: ManagedFileGuard,
    size: u64,
    sha1: [u8; 20],
}

impl ComponentLane {
    pub(crate) fn prepare_fresh(
        lease: &ManagedRootPublicationLease,
        component: ManagedComponentKind,
    ) -> Result<Self, ComponentEffectsError> {
        Self::prepare_fresh_inner(lease, component, || {})
    }

    #[cfg(test)]
    fn prepare_fresh_with_cleanup_hook(
        lease: &ManagedRootPublicationLease,
        component: ManagedComponentKind,
        after_cleanup_admission: impl FnOnce(),
    ) -> Result<Self, ComponentEffectsError> {
        Self::prepare_fresh_inner(lease, component, after_cleanup_admission)
    }

    fn prepare_fresh_inner(
        lease: &ManagedRootPublicationLease,
        component: ManagedComponentKind,
        after_cleanup_admission: impl FnOnce(),
    ) -> Result<Self, ComponentEffectsError> {
        lease.revalidate()?;
        let publication = lease.publication_directory();
        let lane_name = component_lane_name(component);
        let lane = open_or_create_exact_child(publication, lane_name)?;
        let cleanup = ComponentPreintentCleanupPlan::admit(&lane, component)?;
        after_cleanup_admission();
        lease.revalidate()?;
        cleanup.execute(&lane)?;
        let table = open_or_create_exact_child(&lane, COMPONENT_TABLE_DIRECTORY)?;
        let staging = open_or_create_exact_child(&lane, COMPONENT_STAGING_DIRECTORY)?;
        let quarantine = open_or_create_exact_child(&lane, COMPONENT_QUARANTINE_DIRECTORY)?;
        let ancestors = open_or_create_exact_child(&lane, COMPONENT_ANCESTORS_DIRECTORY)?;
        let ancestor_records =
            open_or_create_exact_child(&ancestors, COMPONENT_ANCESTOR_RECORDS_DIRECTORY)?;
        let ancestor_staging =
            open_or_create_exact_child(&ancestors, COMPONENT_ANCESTOR_STAGING_DIRECTORY)?;
        if !exact_entry_names(&table, 1)?.is_empty()
            || !exact_entry_names(&staging, 1)?.is_empty()
            || !exact_entry_names(&quarantine, 1)?.is_empty()
            || exact_entry_names(&ancestors, 3)?
                != BTreeSet::from([
                    COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
                    COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
                ])
            || !exact_entry_names(&ancestor_records, 1)?.is_empty()
            || !exact_entry_names(&ancestor_staging, 1)?.is_empty()
        {
            return Err(ComponentEffectsError::Topology);
        }
        table.sync()?;
        staging.sync()?;
        quarantine.sync()?;
        ancestor_records.sync()?;
        ancestor_staging.sync()?;
        ancestors.sync()?;
        lane.sync()?;
        publication.sync()?;
        lease.root().sync()?;
        lease.revalidate()?;

        Ok(Self {
            component,
            lane,
            table,
            staging,
            quarantine,
            ancestors,
            ancestor_records,
            ancestor_staging,
        })
    }

    pub(crate) fn component(&self) -> ManagedComponentKind {
        self.component
    }

    pub(crate) fn lane(&self) -> &ManagedDir {
        &self.lane
    }

    pub(crate) fn staging(&self) -> &ManagedDir {
        &self.staging
    }

    pub(crate) fn quarantine(&self) -> &ManagedDir {
        &self.quarantine
    }

    pub(crate) fn create_shard_buckets(
        &self,
        shard_index: usize,
    ) -> Result<ComponentShardBuckets, ComponentEffectsError> {
        let name = component_bucket_name(shard_index)?;
        let staging = self.staging.create_child_new(&name)?;
        self.staging.sync()?;
        let quarantine = self.quarantine.create_child_new(&name)?;
        self.quarantine.sync()?;
        self.lane.sync()?;
        Ok(ComponentShardBuckets {
            staging,
            quarantine,
        })
    }

    pub(crate) fn publish_table(
        &self,
        mut replay: ComponentTableReplay,
        manifest: &ComponentIntentManifest,
    ) -> Result<ComponentTableSummary, ComponentEffectsError> {
        if manifest.component != self.component || !exact_entry_names(&self.table, 1)?.is_empty() {
            return Err(ComponentEffectsError::Topology);
        }
        let mut parser = ComponentTableParser::new(manifest.clone())?;
        let mut next_shard = 0_usize;
        while let Some((descriptor, encoded)) = replay.next()? {
            let expected = manifest
                .shards
                .get(next_shard)
                .ok_or(ComponentEffectsError::Topology)?;
            if &descriptor != expected {
                return Err(ComponentEffectsError::Topology);
            }
            let name = component_table_file_name(next_shard)?;
            let guard = self.table.write_new_exact_guarded(&name, &encoded)?;
            let validation = (|| -> Result<(), ComponentEffectsError> {
                if guard.size() != u64::from(descriptor.byte_len)
                    || guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64
                {
                    return Err(ComponentEffectsError::Topology);
                }
                let durable = self.table.read_guarded_file_bounded(
                    &name,
                    &guard,
                    MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
                )?;
                parser.parse_next(&durable)?;
                Ok(())
            })();
            if let Err(error) = validation {
                self.table.remove_guarded_file(&name, &guard)?;
                self.table.sync()?;
                return Err(error);
            }
            self.table.sync()?;
            next_shard += 1;
        }
        if next_shard != manifest.shards.len() {
            return Err(ComponentEffectsError::Topology);
        }
        self.lane.sync()?;
        parser.finish().map_err(Into::into)
    }

    pub(crate) fn into_intent_candidate(
        self,
        lease: ManagedRootPublicationLease,
        manifest: ComponentIntentManifest,
    ) -> Result<ComponentIntentCandidate, ComponentEffectsError> {
        lease.revalidate()?;
        let expected_lane = lease
            .publication_directory()
            .open_child(component_lane_name(self.component))?;
        if expected_lane.identity()? != self.lane.identity()?
            || manifest.component != self.component
        {
            return Err(ComponentEffectsError::Topology);
        }
        let encoded_intent = encode_component_intent_manifest(&manifest)?;
        let (summary, authority) = admit_component_preintent(&self, &lease, &manifest)?;
        Ok(ComponentIntentCandidate {
            lane: self,
            lease,
            manifest,
            encoded_intent,
            summary,
            authority,
        })
    }
}

impl ComponentShardBuckets {
    pub(crate) fn staging(&self) -> &ManagedDir {
        &self.staging
    }

    pub(crate) fn quarantine(&self) -> &ManagedDir {
        &self.quarantine
    }
}

impl ComponentIntentCandidate {
    pub(crate) fn publish_intent(
        self,
    ) -> Result<ComponentIntentPublished, ComponentIntentPublishFailure> {
        self.publish_intent_inner(
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn publish_intent_with_fault(
        self,
        fault: ComponentIntentPublishFault,
    ) -> Result<ComponentIntentPublished, ComponentIntentPublishFailure> {
        self.publish_intent_inner(Some(fault))
    }

    fn publish_intent_inner(
        self,
        #[cfg(test)] fault: Option<ComponentIntentPublishFault>,
    ) -> Result<ComponentIntentPublished, ComponentIntentPublishFailure> {
        let (current_summary, current_authority) =
            match admit_component_preintent(&self.lane, &self.lease, &self.manifest) {
                Ok(admitted) => admitted,
                Err(cause) => {
                    return Err(ComponentIntentPublishFailure::BeforePromotion {
                        candidate: Box::new(self),
                        cause,
                    });
                }
            };
        if current_summary != self.summary || current_authority != self.authority {
            return Err(ComponentIntentPublishFailure::BeforePromotion {
                candidate: Box::new(self),
                cause: ComponentEffectsError::Topology,
            });
        }
        #[cfg(test)]
        if fault == Some(ComponentIntentPublishFault::PromotionAttemptedWithoutMarker) {
            return Err(ComponentIntentPublishFailure::PromotionAttempted {
                candidate: Box::new(self),
                intent_guard: None,
            });
        }
        #[cfg(test)]
        let marker_result = match fault {
            Some(ComponentIntentPublishFault::BeforeMarkerPromotion) => {
                self.lane.lane.write_new_exact_retained_with_fault(
                    COMPONENT_INTENT_FILE,
                    &self.encoded_intent,
                    ManagedCreateOnlyWriteFault::AfterTempVerified,
                )
            }
            Some(ComponentIntentPublishFault::AfterMarkerPromotion) => {
                self.lane.lane.write_new_exact_retained_with_fault(
                    COMPONENT_INTENT_FILE,
                    &self.encoded_intent,
                    ManagedCreateOnlyWriteFault::AfterPromotion,
                )
            }
            _ => self
                .lane
                .lane
                .write_new_exact_retained(COMPONENT_INTENT_FILE, &self.encoded_intent),
        };
        #[cfg(not(test))]
        let marker_result = self
            .lane
            .lane
            .write_new_exact_retained(COMPONENT_INTENT_FILE, &self.encoded_intent);
        let intent_guard = match marker_result {
            Ok(guard) => guard,
            Err(ManagedCreateOnlyWriteFailure::BeforePromotion(cause)) => {
                return Err(ComponentIntentPublishFailure::BeforePromotion {
                    candidate: Box::new(self),
                    cause: cause.into(),
                });
            }
            Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
                return Err(ComponentIntentPublishFailure::PromotionAttempted {
                    candidate: Box::new(self),
                    intent_guard: final_guard,
                });
            }
        };
        if finish_component_intent_publication(
            &self,
            &intent_guard,
            #[cfg(test)]
            fault,
        )
        .is_err()
        {
            return Err(ComponentIntentPublishFailure::PromotionAttempted {
                candidate: Box::new(self),
                intent_guard: Some(intent_guard),
            });
        }
        drop((current_summary, current_authority));
        let Self {
            lane,
            lease,
            manifest,
            encoded_intent,
            summary,
            authority,
        } = self;
        drop((summary, authority));
        Ok(ComponentIntentPublished {
            lane,
            lease,
            manifest,
            encoded_intent,
            intent_guard,
        })
    }
}

impl ComponentCanonicalPathPlan {
    pub(crate) fn first_created_depth(&self) -> Option<u16> {
        self.first_created_depth
    }

    pub(crate) fn creation_anchor(&self) -> &ManagedDir {
        &self.creation_anchor
    }

    pub(crate) fn remaining_parent_segments(&self) -> &[String] {
        &self.remaining_parent_segments
    }

    pub(crate) fn parent(&self) -> Option<&ManagedDir> {
        self.remaining_parent_segments
            .is_empty()
            .then_some(&self.creation_anchor)
    }

    pub(crate) fn file_name(&self) -> &str {
        &self.file_name
    }

    pub(crate) fn observe(&self) -> Result<ComponentCanonicalObservation, ComponentEffectsError> {
        if let Some(first_missing) = self.remaining_parent_segments.first() {
            if self
                .creation_anchor
                .has_portably_exact_child_name(first_missing)?
            {
                return Err(ComponentEffectsError::Topology);
            }
            return Ok(ComponentCanonicalObservation::Absent);
        }
        let parent = &self.creation_anchor;
        let _ = parent.has_portably_exact_child_name(&self.file_name)?;
        let Some(guard) = parent.inspect_regular_file(&self.file_name)? else {
            return Ok(ComponentCanonicalObservation::Absent);
        };
        let size = guard.size();
        if size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(ComponentEffectsError::Topology);
        }
        let sha1 =
            parent.sha1_guarded_file_bytes(&self.file_name, &guard, MAX_TIER2_ARTIFACT_BYTES)?;
        Ok(ComponentCanonicalObservation::Regular(
            ComponentObservedFile {
                parent: parent.clone(),
                file_name: self.file_name.clone(),
                guard,
                size,
                sha1,
            },
        ))
    }
}

impl ComponentObservedFile {
    pub(crate) fn parent(&self) -> &ManagedDir {
        &self.parent
    }

    pub(crate) fn file_name(&self) -> &str {
        &self.file_name
    }

    pub(crate) fn guard(&self) -> &ManagedFileGuard {
        &self.guard
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn sha1(&self) -> [u8; 20] {
        self.sha1
    }
}

pub(crate) fn plan_component_canonical_path(
    root: &ManagedDir,
    component: ManagedComponentKind,
    relative: &ArtifactRelativePath,
) -> Result<ComponentCanonicalPathPlan, ComponentEffectsError> {
    root.revalidate()?;
    let segment_count = relative.as_str().split('/').count();
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| ComponentEffectsError::Topology)?;
    segments.extend(relative.as_str().split('/'));
    let file_name = copy_bounded_string(segments.pop().ok_or(ComponentEffectsError::Topology)?)?;
    let component_root_name = component_lane_name(component);
    if !root.has_portably_exact_child_name(component_root_name)? {
        let parent_count = segments
            .len()
            .checked_add(1)
            .ok_or(ComponentEffectsError::Topology)?;
        let mut remaining_parent_segments = Vec::new();
        remaining_parent_segments
            .try_reserve_exact(parent_count)
            .map_err(|_| ComponentEffectsError::Topology)?;
        remaining_parent_segments.push(copy_bounded_string(component_root_name)?);
        for segment in segments {
            remaining_parent_segments.push(copy_bounded_string(segment)?);
        }
        return Ok(ComponentCanonicalPathPlan {
            creation_anchor: root.clone(),
            remaining_parent_segments,
            file_name,
            first_created_depth: Some(0),
        });
    }

    let mut parent = root.open_child(component_root_name)?;
    for (index, segment) in segments.iter().copied().enumerate() {
        if !parent.has_portably_exact_child_name(segment)? {
            let mut remaining_parent_segments = Vec::new();
            remaining_parent_segments
                .try_reserve_exact(segments.len() - index)
                .map_err(|_| ComponentEffectsError::Topology)?;
            for missing in &segments[index..] {
                remaining_parent_segments.push(copy_bounded_string(missing)?);
            }
            return Ok(ComponentCanonicalPathPlan {
                creation_anchor: parent,
                remaining_parent_segments,
                file_name,
                first_created_depth: Some(
                    u16::try_from(index + 1).map_err(|_| ComponentEffectsError::Topology)?,
                ),
            });
        }
        parent = parent.open_child(segment)?;
    }
    // Reject a portable alias during planning; observation repeats the check.
    let _ = parent.has_portably_exact_child_name(&file_name)?;
    Ok(ComponentCanonicalPathPlan {
        creation_anchor: parent,
        remaining_parent_segments: Vec::new(),
        file_name,
        first_created_depth: None,
    })
}

pub(crate) fn component_root_binding_sha256(
    root: &ManagedDir,
) -> Result<[u8; 32], ComponentEffectsError> {
    let binding = root.identity()?.persistent_binding();
    Ok(Sha256::digest(binding.as_bytes()).into())
}

fn admit_component_preintent(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    manifest: &ComponentIntentManifest,
) -> Result<(ComponentTableSummary, ComponentPreintentAuthority), ComponentEffectsError> {
    lease.revalidate()?;
    if manifest.component != lane.component
        || component_root_binding_sha256(lease.root())? != manifest.root_binding_sha256
    {
        return Err(ComponentEffectsError::Topology);
    }
    let expected_lane = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    if exact_entry_names(&lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1)? != expected_lane {
        return Err(ComponentEffectsError::Topology);
    }
    validate_empty_ancestor_topology(lane)?;
    validate_indexed_names(
        &lane.table,
        manifest.shards.len(),
        MAX_COMPONENT_TABLE_SHARDS,
        component_table_file_name,
    )?;
    validate_indexed_names(
        &lane.staging,
        manifest.shards.len(),
        MAX_COMPONENT_TABLE_SHARDS,
        component_bucket_name,
    )?;
    validate_indexed_names(
        &lane.quarantine,
        manifest.shards.len(),
        MAX_COMPONENT_TABLE_SHARDS,
        component_bucket_name,
    )?;

    let mut parser = ComponentTableParser::new(manifest.clone())?;
    let mut shard_authority = Vec::new();
    shard_authority
        .try_reserve_exact(manifest.shards.len())
        .map_err(|_| ComponentEffectsError::Topology)?;
    for (shard_index, descriptor) in manifest.shards.iter().enumerate() {
        let table_name = component_table_file_name(shard_index)?;
        let table_guard = lane
            .table
            .inspect_regular_file(&table_name)?
            .ok_or(ComponentEffectsError::Topology)?;
        if table_guard.size() != u64::from(descriptor.byte_len)
            || table_guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64
        {
            return Err(ComponentEffectsError::Topology);
        }
        let encoded = lane.table.read_guarded_file_bounded(
            &table_name,
            &table_guard,
            MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
        )?;
        let shard = parser.parse_next(&encoded)?;
        let table_identity = table_guard.identity();
        drop(table_guard);

        let bucket_name = component_bucket_name(shard_index)?;
        let staging = lane.staging.open_child(&bucket_name)?;
        let quarantine = lane.quarantine.open_child(&bucket_name)?;
        let staging_identity = staging.identity()?;
        let quarantine_identity = quarantine.identity()?;
        let staging_names = exact_entry_names(&staging, 257)?;
        if !exact_entry_names(&quarantine, 1)?.is_empty() {
            return Err(ComponentEffectsError::Topology);
        }
        let mut expected_staged = 0_usize;
        let mut row_authority = Vec::new();
        row_authority
            .try_reserve_exact(shard.rows.len())
            .map_err(|_| ComponentEffectsError::Topology)?;
        for (row_index, row) in shard.rows.iter().enumerate() {
            row_authority.push(admit_component_preintent_row(
                lease.root(),
                lane.component,
                row,
                &staging,
                &staging_names,
                row_index,
            )?);
            if !row.prior_is_final() {
                expected_staged += 1;
            }
        }
        if staging_names.len() != expected_staged {
            return Err(ComponentEffectsError::Topology);
        }
        staging.sync()?;
        quarantine.sync()?;
        shard_authority.push(ComponentPreintentShardAuthority {
            table: table_identity,
            staging: staging_identity,
            quarantine: quarantine_identity,
            rows: row_authority,
        });
    }
    let summary = parser.finish()?;
    lane.table.sync()?;
    lane.staging.sync()?;
    lane.quarantine.sync()?;
    lane.ancestor_records.sync()?;
    lane.ancestor_staging.sync()?;
    lane.ancestors.sync()?;
    lane.lane.sync()?;
    lease.publication_directory().sync()?;
    lease.root().sync()?;
    lease.revalidate()?;
    if component_root_binding_sha256(lease.root())? != manifest.root_binding_sha256 {
        return Err(ComponentEffectsError::Topology);
    }
    Ok((
        summary,
        ComponentPreintentAuthority {
            root: lease.root().identity()?,
            publication: lease.publication_directory().identity()?,
            lane: lane.lane.identity()?,
            table: lane.table.identity()?,
            staging: lane.staging.identity()?,
            quarantine: lane.quarantine.identity()?,
            ancestors: lane.ancestors.identity()?,
            ancestor_records: lane.ancestor_records.identity()?,
            ancestor_staging: lane.ancestor_staging.identity()?,
            shards: shard_authority,
        },
    ))
}

fn admit_component_preintent_row(
    root: &ManagedDir,
    component: ManagedComponentKind,
    row: &ComponentTableRow,
    staging: &ManagedDir,
    staging_names: &BTreeSet<String>,
    row_index: usize,
) -> Result<ComponentPreintentRowAuthority, ComponentEffectsError> {
    let slot_name = component_slot_name(row_index)?;
    let staged = staging_names.contains(&slot_name);
    let mut staging_identity = None;
    if row.prior_is_final() {
        if staged {
            return Err(ComponentEffectsError::Topology);
        }
    } else {
        if !staged {
            return Err(ComponentEffectsError::Topology);
        }
        let guard = staging
            .inspect_regular_file(&slot_name)?
            .ok_or(ComponentEffectsError::Topology)?;
        if guard.size() != row.final_size
            || staging.sha1_guarded_file_bytes(&slot_name, &guard, MAX_TIER2_ARTIFACT_BYTES)?
                != row.final_sha1
        {
            return Err(ComponentEffectsError::Topology);
        }
        staging_identity = Some(guard.identity());
    }

    let canonical = plan_component_canonical_path(root, component, &row.path)?;
    if canonical.first_created_depth() != row.first_created_depth {
        return Err(ComponentEffectsError::Topology);
    }
    let canonical_anchor = canonical.creation_anchor().identity()?;
    let canonical_identity = match (&row.prior, canonical.observe()?) {
        (None, ComponentCanonicalObservation::Absent) => None,
        (Some(prior), ComponentCanonicalObservation::Regular(observed))
            if observed.size() == prior.size && observed.sha1() == prior.sha1 =>
        {
            Some(observed.guard().identity())
        }
        _ => return Err(ComponentEffectsError::Topology),
    };
    Ok(ComponentPreintentRowAuthority {
        staging: staging_identity,
        canonical_anchor,
        canonical: canonical_identity,
    })
}

fn validate_indexed_names(
    directory: &ManagedDir,
    expected_count: usize,
    maximum: usize,
    expected_name: impl Fn(usize) -> Result<String, ComponentEffectsError>,
) -> Result<(), ComponentEffectsError> {
    if expected_count > maximum {
        return Err(ComponentEffectsError::Topology);
    }
    let names = exact_entry_names(directory, maximum + 1)?;
    if names.len() != expected_count {
        return Err(ComponentEffectsError::Topology);
    }
    for (index, name) in names.iter().enumerate() {
        if *name != expected_name(index)? {
            return Err(ComponentEffectsError::Topology);
        }
    }
    Ok(())
}

fn finish_component_intent_publication(
    candidate: &ComponentIntentCandidate,
    intent_guard: &ManagedFileGuard,
    #[cfg(test)] fault: Option<ComponentIntentPublishFault>,
) -> Result<(), ComponentEffectsError> {
    if intent_guard.size()
        != u64::try_from(candidate.encoded_intent.len())
            .map_err(|_| ComponentEffectsError::Topology)?
        || candidate.lane.lane.read_guarded_file_bounded(
            COMPONENT_INTENT_FILE,
            intent_guard,
            MAX_COMPONENT_INTENT_BYTES as u64,
        )? != candidate.encoded_intent
    {
        return Err(ComponentEffectsError::Topology);
    }
    let expected_lane = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_INTENT_FILE.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    if exact_entry_names(&candidate.lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1)? != expected_lane {
        return Err(ComponentEffectsError::Topology);
    }
    validate_empty_ancestor_topology(&candidate.lane)?;
    candidate.lane.lane.sync()?;
    #[cfg(test)]
    if fault == Some(ComponentIntentPublishFault::AfterLaneSynced) {
        return Err(ComponentEffectsError::Topology);
    }
    candidate.lease.publication_directory().sync()?;
    #[cfg(test)]
    if fault == Some(ComponentIntentPublishFault::AfterPublicationSynced) {
        return Err(ComponentEffectsError::Topology);
    }
    candidate.lease.root().sync()?;
    #[cfg(test)]
    if fault == Some(ComponentIntentPublishFault::AfterRootSynced) {
        return Err(ComponentEffectsError::Topology);
    }
    candidate.lease.revalidate()?;
    #[cfg(test)]
    if fault == Some(ComponentIntentPublishFault::AfterLeaseRevalidated) {
        return Err(ComponentEffectsError::Topology);
    }
    if component_root_binding_sha256(candidate.lease.root())?
        != candidate.manifest.root_binding_sha256
        || !candidate
            .lane
            .lane
            .file_guard_matches(COMPONENT_INTENT_FILE, intent_guard)?
    {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(())
}

fn validate_empty_ancestor_topology(lane: &ComponentLane) -> Result<(), ComponentEffectsError> {
    let ancestors = lane.lane.open_child(COMPONENT_ANCESTORS_DIRECTORY)?;
    let records = ancestors.open_child(COMPONENT_ANCESTOR_RECORDS_DIRECTORY)?;
    let staging = ancestors.open_child(COMPONENT_ANCESTOR_STAGING_DIRECTORY)?;
    if ancestors.identity()? != lane.ancestors.identity()?
        || records.identity()? != lane.ancestor_records.identity()?
        || staging.identity()? != lane.ancestor_staging.identity()?
        || exact_entry_names(&ancestors, 3)?
            != BTreeSet::from([
                COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
                COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
            ])
        || !exact_entry_names(&records, 1)?.is_empty()
        || !exact_entry_names(&staging, 1)?.is_empty()
    {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(())
}

fn copy_bounded_string(value: &str) -> Result<String, ComponentEffectsError> {
    let mut copied = String::new();
    copied
        .try_reserve_exact(value.len())
        .map_err(|_| ComponentEffectsError::Topology)?;
    copied.push_str(value);
    Ok(copied)
}

impl ComponentPreintentCleanupPlan {
    fn admit(
        lane: &ManagedDir,
        component: ManagedComponentKind,
    ) -> Result<Self, ComponentEffectsError> {
        let entries = plan_component_lane_entries(lane)?;
        let names = &entries.directories;
        let table = names
            .contains(COMPONENT_TABLE_DIRECTORY)
            .then(|| lane.open_child(COMPONENT_TABLE_DIRECTORY))
            .transpose()?
            .map(|directory| ComponentTableCleanupPlan::admit(directory, component))
            .transpose()?;
        let staging = names
            .contains(COMPONENT_STAGING_DIRECTORY)
            .then(|| lane.open_child(COMPONENT_STAGING_DIRECTORY))
            .transpose()?
            .map(ComponentBucketCleanupPlan::admit)
            .transpose()?;
        let quarantine = names
            .contains(COMPONENT_QUARANTINE_DIRECTORY)
            .then(|| lane.open_child(COMPONENT_QUARANTINE_DIRECTORY))
            .transpose()?
            .map(ComponentBucketCleanupPlan::admit)
            .transpose()?;
        let ancestors = names
            .contains(COMPONENT_ANCESTORS_DIRECTORY)
            .then(|| ComponentAncestorsCleanupPlan::admit(lane))
            .transpose()?;
        let plan = Self {
            temporary: entries.temporary,
            table,
            staging,
            quarantine,
            ancestors,
        };
        plan.revalidate_with_temps(lane)?;
        Ok(plan)
    }

    fn revalidate_lane(&self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        let mut expected = BTreeSet::new();
        if self.table.is_some() {
            expected.insert(COMPONENT_TABLE_DIRECTORY.to_string());
        }
        if self.staging.is_some() {
            expected.insert(COMPONENT_STAGING_DIRECTORY.to_string());
        }
        if self.quarantine.is_some() {
            expected.insert(COMPONENT_QUARANTINE_DIRECTORY.to_string());
        }
        if self.ancestors.is_some() {
            expected.insert(COMPONENT_ANCESTORS_DIRECTORY.to_string());
        }
        if exact_entry_names(lane, MAX_COMPONENT_LANE_ENTRIES + 1)? != expected {
            return Err(ComponentEffectsError::Topology);
        }
        Ok(())
    }

    fn revalidate_lane_with_temps(&self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        let mut expected = BTreeSet::new();
        if self.table.is_some() {
            expected.insert(COMPONENT_TABLE_DIRECTORY.to_string());
        }
        if self.staging.is_some() {
            expected.insert(COMPONENT_STAGING_DIRECTORY.to_string());
        }
        if self.quarantine.is_some() {
            expected.insert(COMPONENT_QUARANTINE_DIRECTORY.to_string());
        }
        if self.ancestors.is_some() {
            expected.insert(COMPONENT_ANCESTORS_DIRECTORY.to_string());
        }
        for file in &self.temporary {
            expected.insert(file.name.clone());
        }
        if exact_entry_names(
            lane,
            MAX_COMPONENT_LANE_ENTRIES + MAX_MANAGED_TEMP_ENTRIES + 1,
        )? != expected
        {
            return Err(ComponentEffectsError::Topology);
        }
        for file in &self.temporary {
            let guard = inspect_planned_file(lane, file)?;
            if !validate_managed_temp_name(&file.name)?
                || !lane.managed_temp_is_orphan(&file.name, &guard)?
            {
                return Err(ComponentEffectsError::Topology);
            }
        }
        Ok(())
    }

    fn revalidate_with_temps(&self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        self.revalidate_lane_with_temps(lane)?;
        if let Some(table) = &self.table {
            table.revalidate_with_temps()?;
        }
        if let Some(staging) = &self.staging {
            staging.revalidate_with_temps()?;
        }
        if let Some(quarantine) = &self.quarantine {
            quarantine.revalidate_with_temps()?;
        }
        if let Some(ancestors) = &self.ancestors {
            ancestors.revalidate(lane)?;
        }
        Ok(())
    }

    fn revalidate(&self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        self.revalidate_lane(lane)?;
        if let Some(table) = &self.table {
            table.revalidate()?;
        }
        if let Some(staging) = &self.staging {
            staging.revalidate()?;
        }
        if let Some(quarantine) = &self.quarantine {
            quarantine.revalidate()?;
        }
        if let Some(ancestors) = &self.ancestors {
            ancestors.revalidate(lane)?;
        }
        Ok(())
    }

    fn execute(self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        self.revalidate_with_temps(lane)?;
        remove_planned_temps(lane, &self.temporary)?;
        if let Some(table) = &self.table {
            table.remove_temps()?;
        }
        if let Some(staging) = &self.staging {
            staging.remove_temps()?;
        }
        if let Some(quarantine) = &self.quarantine {
            quarantine.remove_temps()?;
        }
        self.revalidate(lane)?;
        if let Some(table) = self.table {
            table.execute()?;
        }
        if let Some(quarantine) = self.quarantine {
            quarantine.execute()?;
        }
        if let Some(staging) = self.staging {
            staging.execute()?;
        }
        Ok(())
    }
}

impl ComponentAncestorsCleanupPlan {
    fn admit(lane: &ManagedDir) -> Result<Self, ComponentEffectsError> {
        let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY)?;
        let names = exact_entry_names(&ancestors, 3)?;
        if names.iter().any(|name| {
            name != COMPONENT_ANCESTOR_RECORDS_DIRECTORY
                && name != COMPONENT_ANCESTOR_STAGING_DIRECTORY
        }) {
            return Err(ComponentEffectsError::Topology);
        }
        let records = names
            .contains(COMPONENT_ANCESTOR_RECORDS_DIRECTORY)
            .then(|| ancestors.open_child(COMPONENT_ANCESTOR_RECORDS_DIRECTORY))
            .transpose()?
            .map(|records| {
                let identity = records.identity()?;
                Ok::<_, ComponentEffectsError>((records, identity))
            })
            .transpose()?;
        let staging = names
            .contains(COMPONENT_ANCESTOR_STAGING_DIRECTORY)
            .then(|| ancestors.open_child(COMPONENT_ANCESTOR_STAGING_DIRECTORY))
            .transpose()?
            .map(|staging| {
                let identity = staging.identity()?;
                Ok::<_, ComponentEffectsError>((staging, identity))
            })
            .transpose()?;
        let plan = Self {
            ancestors_identity: ancestors.identity()?,
            ancestors,
            records,
            staging,
        };
        plan.revalidate(lane)?;
        Ok(plan)
    }

    fn revalidate(&self, lane: &ManagedDir) -> Result<(), ComponentEffectsError> {
        let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY)?;
        let mut expected = BTreeSet::new();
        if self.records.is_some() {
            expected.insert(COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string());
        }
        if self.staging.is_some() {
            expected.insert(COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string());
        }
        if ancestors.identity()? != self.ancestors_identity
            || self.ancestors.identity()? != self.ancestors_identity
            || exact_entry_names(&ancestors, 3)? != expected
        {
            return Err(ComponentEffectsError::Topology);
        }
        for (name, retained) in [
            (COMPONENT_ANCESTOR_RECORDS_DIRECTORY, &self.records),
            (COMPONENT_ANCESTOR_STAGING_DIRECTORY, &self.staging),
        ] {
            let Some((retained, identity)) = retained else {
                continue;
            };
            let current = ancestors.open_child(name)?;
            if current.identity()? != *identity
                || retained.identity()? != *identity
                || !exact_entry_names(&current, 1)?.is_empty()
            {
                return Err(ComponentEffectsError::Topology);
            }
        }
        Ok(())
    }
}

impl ComponentTableCleanupPlan {
    fn admit(
        directory: ManagedDir,
        component: ManagedComponentKind,
    ) -> Result<Self, ComponentEffectsError> {
        let planned = plan_directory_files(
            &directory,
            MAX_COMPONENT_TABLE_SHARDS,
            MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
            |name| parse_component_table_file_name(name).is_some(),
        )?;
        validate_component_table_prefix(&directory, component, &planned.owned)?;
        Ok(Self {
            directory,
            component,
            files: planned.owned,
            temporary: planned.temporary,
        })
    }

    fn revalidate_with_temps(&self) -> Result<(), ComponentEffectsError> {
        validate_planned_file_entries(
            &self.directory,
            &self.files,
            &self.temporary,
            MAX_COMPONENT_TABLE_SHARDS,
            true,
        )?;
        validate_component_table_prefix(&self.directory, self.component, &self.files)
    }

    fn revalidate(&self) -> Result<(), ComponentEffectsError> {
        validate_planned_file_entries(
            &self.directory,
            &self.files,
            &[],
            MAX_COMPONENT_TABLE_SHARDS,
            false,
        )?;
        validate_component_table_prefix(&self.directory, self.component, &self.files)
    }

    fn remove_temps(&self) -> Result<(), ComponentEffectsError> {
        remove_planned_temps(&self.directory, &self.temporary)
    }

    fn execute(self) -> Result<(), ComponentEffectsError> {
        self.revalidate()?;
        for (index, file) in self.files.iter().enumerate().rev() {
            let guard = inspect_planned_file(&self.directory, file)?;
            let encoded = self.directory.read_guarded_file_bounded(
                &file.name,
                &guard,
                MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
            )?;
            let shard = decode_component_table_shard(&encoded)?;
            if usize::try_from(shard.shard_index).map_err(|_| ComponentEffectsError::Topology)?
                != index
            {
                return Err(ComponentEffectsError::Topology);
            }
            let guard = inspect_planned_file(&self.directory, file)?;
            self.directory.remove_guarded_file(&file.name, &guard)?;
            self.directory.sync()?;
        }
        Ok(())
    }
}

fn validate_component_table_prefix(
    directory: &ManagedDir,
    component: ManagedComponentKind,
    files: &[ComponentPlannedFile],
) -> Result<(), ComponentEffectsError> {
    let mut transaction_binding = None;
    for (index, file) in files.iter().enumerate() {
        if file.name != component_table_file_name(index)? {
            return Err(ComponentEffectsError::Topology);
        }
        let guard = inspect_planned_file(directory, file)?;
        if guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64 {
            return Err(ComponentEffectsError::Topology);
        }
        let encoded = directory.read_guarded_file_bounded(
            &file.name,
            &guard,
            MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
        )?;
        let shard = decode_component_table_shard(&encoded)?;
        let binding = (
            shard.shard_count,
            shard.total_rows,
            shard.transaction_nonce,
            shard.root_binding_sha256,
        );
        if shard.component != component
            || usize::try_from(shard.shard_index).map_err(|_| ComponentEffectsError::Topology)?
                != index
            || usize::try_from(shard.shard_count).map_err(|_| ComponentEffectsError::Topology)?
                < index + 1
            || transaction_binding.is_some_and(|expected| expected != binding)
        {
            return Err(ComponentEffectsError::Topology);
        }
        transaction_binding.get_or_insert(binding);
    }
    Ok(())
}

impl ComponentBucketCleanupPlan {
    fn admit(directory: ManagedDir) -> Result<Self, ComponentEffectsError> {
        let names = exact_entry_names(&directory, MAX_COMPONENT_TABLE_SHARDS + 2)?;
        let mut buckets = Vec::new();
        buckets
            .try_reserve_exact(names.len().min(MAX_COMPONENT_TABLE_SHARDS))
            .map_err(|_| ComponentEffectsError::Topology)?;
        let mut parked = None;
        let mut total_slots = 0_usize;
        let mut total_bytes = 0_u64;
        for name in names {
            if name == COMPONENT_BUCKET_PARK_A || name == COMPONENT_BUCKET_PARK_B {
                if parked.is_some() {
                    return Err(ComponentEffectsError::Topology);
                }
                let (name, alternate) = if name == COMPONENT_BUCKET_PARK_A {
                    (COMPONENT_BUCKET_PARK_A, COMPONENT_BUCKET_PARK_B)
                } else {
                    (COMPONENT_BUCKET_PARK_B, COMPONENT_BUCKET_PARK_A)
                };
                let child = directory.open_child(name)?;
                if !exact_entry_names(&child, 1)?.is_empty() {
                    return Err(ComponentEffectsError::Topology);
                }
                parked = Some(ComponentParkedBucket {
                    name,
                    alternate,
                    identity: child.identity()?,
                });
                continue;
            }
            let shard_index =
                parse_component_bucket_name(&name).ok_or(ComponentEffectsError::Topology)?;
            let child = directory.open_child(&name)?;
            let identity = child.identity()?;
            let planned = plan_directory_files(&child, 256, MAX_TIER2_ARTIFACT_BYTES, |slot| {
                parse_component_slot_name(slot).is_some()
            })?;
            for file in &planned.owned {
                total_slots = total_slots
                    .checked_add(1)
                    .filter(|count| *count <= MAX_TIER2_ENTRIES)
                    .ok_or(ComponentEffectsError::Topology)?;
                total_bytes = total_bytes
                    .checked_add(file.size)
                    .filter(|bytes| *bytes <= MAX_TIER2_AGGREGATE_BYTES)
                    .ok_or(ComponentEffectsError::Topology)?;
            }
            if shard_index >= MAX_COMPONENT_TABLE_SHARDS {
                return Err(ComponentEffectsError::Topology);
            }
            buckets.push(ComponentGuardedBucket {
                name,
                identity,
                files: planned.owned,
                temporary: planned.temporary,
            });
        }
        Ok(Self {
            directory,
            buckets,
            parked,
        })
    }

    fn remove_temps(&self) -> Result<(), ComponentEffectsError> {
        for bucket in &self.buckets {
            let directory = open_planned_child(&self.directory, &bucket.name, bucket.identity)?;
            remove_planned_temps(&directory, &bucket.temporary)?;
        }
        self.directory.sync()?;
        Ok(())
    }

    fn validate_exact(&self, include_temporary: bool) -> Result<(), ComponentEffectsError> {
        let mut expected = self
            .buckets
            .iter()
            .map(|bucket| bucket.name.clone())
            .collect::<BTreeSet<_>>();
        if let Some(parked) = &self.parked {
            expected.insert(parked.name.to_string());
        }
        if exact_entry_names(&self.directory, MAX_COMPONENT_TABLE_SHARDS + 2)? != expected {
            return Err(ComponentEffectsError::Topology);
        }
        for bucket in &self.buckets {
            let directory = open_planned_child(&self.directory, &bucket.name, bucket.identity)?;
            let temporary = include_temporary
                .then_some(bucket.temporary.as_slice())
                .unwrap_or_default();
            validate_planned_file_entries(
                &directory,
                &bucket.files,
                temporary,
                256,
                include_temporary,
            )?;
        }
        if let Some(parked) = &self.parked {
            let directory = open_planned_child(&self.directory, parked.name, parked.identity)?;
            if !exact_entry_names(&directory, 1)?.is_empty() {
                return Err(ComponentEffectsError::Topology);
            }
        }
        Ok(())
    }

    fn revalidate_with_temps(&self) -> Result<(), ComponentEffectsError> {
        self.validate_exact(true)
    }

    fn revalidate(&self) -> Result<(), ComponentEffectsError> {
        self.validate_exact(false)
    }

    fn execute(self) -> Result<(), ComponentEffectsError> {
        self.revalidate()?;
        for bucket in self.buckets.iter().rev() {
            let directory = open_planned_child(&self.directory, &bucket.name, bucket.identity)?;
            for file in bucket.files.iter().rev() {
                let guard = inspect_planned_file(&directory, file)?;
                directory.remove_guarded_file(&file.name, &guard)?;
                directory.sync()?;
            }
        }
        if let Some(parked) = self.parked {
            let directory = open_planned_child(&self.directory, parked.name, parked.identity)?;
            remove_component_bucket(&self.directory, parked.name, parked.alternate, directory)?;
        }
        for bucket in self.buckets.into_iter().rev() {
            let shard_index =
                parse_component_bucket_name(&bucket.name).ok_or(ComponentEffectsError::Topology)?;
            let park = if shard_index % 2 == 0 {
                COMPONENT_BUCKET_PARK_A
            } else {
                COMPONENT_BUCKET_PARK_B
            };
            let directory = open_planned_child(&self.directory, &bucket.name, bucket.identity)?;
            remove_component_bucket(&self.directory, &bucket.name, park, directory)?;
        }
        Ok(())
    }
}

fn remove_component_bucket(
    parent: &ManagedDir,
    name: &str,
    park_name: &str,
    directory: ManagedDir,
) -> Result<(), ComponentEffectsError> {
    let outcome = parent.remove_empty_child_guarded(name, park_name, directory)?;
    parent.sync()?;
    if outcome != ManagedEmptyChildRemoval::Removed {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(())
}

fn open_or_create_exact_child(
    parent: &ManagedDir,
    name: &str,
) -> Result<ManagedDir, ComponentEffectsError> {
    if parent.has_portably_exact_child_name(name)? {
        parent.open_child(name).map_err(Into::into)
    } else {
        parent.create_child_new(name).map_err(Into::into)
    }
}

fn exact_entry_names(
    directory: &ManagedDir,
    limit: usize,
) -> Result<BTreeSet<String>, ComponentEffectsError> {
    if limit == 0 {
        return Err(ComponentEffectsError::Topology);
    }
    let entries = directory.entries_bounded(limit)?;
    if entries.len() >= limit {
        return Err(ComponentEffectsError::Topology);
    }
    entries
        .into_iter()
        .map(|name| {
            name.into_string()
                .map_err(|_| ComponentEffectsError::Topology)
        })
        .collect()
}

fn plan_component_lane_entries(
    lane: &ManagedDir,
) -> Result<ComponentLaneEntryPlan, ComponentEffectsError> {
    let limit = MAX_COMPONENT_LANE_ENTRIES
        .checked_add(MAX_MANAGED_TEMP_ENTRIES)
        .and_then(|entries| entries.checked_add(1))
        .ok_or(ComponentEffectsError::Topology)?;
    let entries = lane.entries_bounded(limit)?;
    if entries.len() >= limit {
        return Err(ComponentEffectsError::Topology);
    }
    let mut directories = BTreeSet::new();
    let mut temporary = Vec::new();
    temporary
        .try_reserve_exact(entries.len().min(MAX_MANAGED_TEMP_ENTRIES))
        .map_err(|_| ComponentEffectsError::Topology)?;
    for name in entries {
        let name = name
            .into_string()
            .map_err(|_| ComponentEffectsError::Topology)?;
        if component_preintent_lane_entry_is_known(&name) {
            directories.insert(name);
            continue;
        }
        if !validate_managed_temp_name(&name)? || temporary.len() >= MAX_MANAGED_TEMP_ENTRIES {
            return Err(ComponentEffectsError::Topology);
        }
        let guard = lane
            .inspect_regular_file(&name)?
            .ok_or(ComponentEffectsError::Topology)?;
        if guard.size() > MAX_COMPONENT_INTENT_BYTES as u64
            || !lane.managed_temp_is_orphan(&name, &guard)?
        {
            return Err(ComponentEffectsError::Topology);
        }
        temporary.push(ComponentPlannedFile {
            name,
            size: guard.size(),
            identity: guard.identity(),
        });
    }
    temporary.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(ComponentLaneEntryPlan {
        directories,
        temporary,
    })
}

fn plan_directory_files(
    directory: &ManagedDir,
    maximum_owned: usize,
    maximum_file_bytes: u64,
    owned_name: impl Fn(&str) -> bool,
) -> Result<ComponentDirectoryFilePlan, ComponentEffectsError> {
    let limit = maximum_owned
        .checked_add(MAX_MANAGED_TEMP_ENTRIES)
        .and_then(|entries| entries.checked_add(1))
        .ok_or(ComponentEffectsError::Topology)?;
    let entries = directory.entries_bounded(limit)?;
    if entries.len() >= limit {
        return Err(ComponentEffectsError::Topology);
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(entries.len().min(maximum_owned))
        .map_err(|_| ComponentEffectsError::Topology)?;
    let mut temporary = Vec::new();
    temporary
        .try_reserve_exact(entries.len().min(MAX_MANAGED_TEMP_ENTRIES))
        .map_err(|_| ComponentEffectsError::Topology)?;
    for name in entries {
        let name = name
            .into_string()
            .map_err(|_| ComponentEffectsError::Topology)?;
        let guard = directory
            .inspect_regular_file(&name)?
            .ok_or(ComponentEffectsError::Topology)?;
        if guard.size() > maximum_file_bytes || !directory.file_guard_matches(&name, &guard)? {
            return Err(ComponentEffectsError::Topology);
        }
        let planned = ComponentPlannedFile {
            name,
            size: guard.size(),
            identity: guard.identity(),
        };
        if validate_managed_temp_name(&planned.name)? {
            if temporary.len() >= MAX_MANAGED_TEMP_ENTRIES
                || !directory.managed_temp_is_orphan(&planned.name, &guard)?
            {
                return Err(ComponentEffectsError::Topology);
            }
            temporary.push(planned);
        } else if owned_name(&planned.name) {
            if owned.len() >= maximum_owned {
                return Err(ComponentEffectsError::Topology);
            }
            owned.push(planned);
        } else {
            return Err(ComponentEffectsError::Topology);
        }
    }
    owned.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    temporary.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(ComponentDirectoryFilePlan { owned, temporary })
}

fn validate_planned_file_entries(
    directory: &ManagedDir,
    owned: &[ComponentPlannedFile],
    temporary: &[ComponentPlannedFile],
    maximum_owned: usize,
    include_temporary: bool,
) -> Result<(), ComponentEffectsError> {
    if owned.len() > maximum_owned
        || temporary.len() > MAX_MANAGED_TEMP_ENTRIES
        || (!include_temporary && !temporary.is_empty())
    {
        return Err(ComponentEffectsError::Topology);
    }
    let expected_count = owned
        .len()
        .checked_add(temporary.len())
        .ok_or(ComponentEffectsError::Topology)?;
    let limit = maximum_owned
        .checked_add(if include_temporary {
            MAX_MANAGED_TEMP_ENTRIES
        } else {
            0
        })
        .and_then(|entries| entries.checked_add(1))
        .ok_or(ComponentEffectsError::Topology)?;
    let names = exact_entry_names(directory, limit)?;
    if names.len() != expected_count
        || owned.iter().any(|file| !names.contains(&file.name))
        || temporary.iter().any(|file| !names.contains(&file.name))
    {
        return Err(ComponentEffectsError::Topology);
    }
    for file in owned {
        let _ = inspect_planned_file(directory, file)?;
    }
    for file in temporary {
        let guard = inspect_planned_file(directory, file)?;
        if !validate_managed_temp_name(&file.name)?
            || !directory.managed_temp_is_orphan(&file.name, &guard)?
        {
            return Err(ComponentEffectsError::Topology);
        }
    }
    Ok(())
}

fn remove_planned_temps(
    directory: &ManagedDir,
    temporary: &[ComponentPlannedFile],
) -> Result<(), ComponentEffectsError> {
    for file in temporary.iter().rev() {
        let guard = inspect_planned_file(directory, file)?;
        if !directory.managed_temp_is_orphan(&file.name, &guard)? {
            return Err(ComponentEffectsError::Topology);
        }
        directory.remove_guarded_file(&file.name, &guard)?;
        directory.sync()?;
    }
    Ok(())
}

fn inspect_planned_file(
    directory: &ManagedDir,
    planned: &ComponentPlannedFile,
) -> Result<ManagedFileGuard, ComponentEffectsError> {
    let guard = directory
        .inspect_regular_file(&planned.name)?
        .ok_or(ComponentEffectsError::Topology)?;
    if guard.size() != planned.size || guard.identity() != planned.identity {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(guard)
}

fn open_planned_child(
    parent: &ManagedDir,
    name: &str,
    identity: ManagedDirectoryIdentity,
) -> Result<ManagedDir, ComponentEffectsError> {
    let directory = parent.open_child(name)?;
    if directory.identity()? != identity {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(directory)
}

fn component_preintent_lane_entry_is_known(name: &str) -> bool {
    matches!(
        name,
        COMPONENT_TABLE_DIRECTORY
            | COMPONENT_STAGING_DIRECTORY
            | COMPONENT_QUARANTINE_DIRECTORY
            | COMPONENT_ANCESTORS_DIRECTORY
    )
}

fn component_bucket_name(index: usize) -> Result<String, ComponentEffectsError> {
    if index >= MAX_COMPONENT_TABLE_SHARDS {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(format!("{index:06}"))
}

fn component_ancestor_bucket_name(index: usize) -> Result<String, ComponentEffectsError> {
    if index >= MAX_COMPONENT_ANCESTOR_SHARDS {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(format!("{index:06}"))
}

fn component_ancestor_record_file_name(index: usize) -> Result<String, ComponentEffectsError> {
    Ok(format!(
        "{}{COMPONENT_ANCESTOR_RECORD_FILE_SUFFIX}",
        component_ancestor_bucket_name(index)?
    ))
}

pub(crate) fn component_slot_name(index: usize) -> Result<String, ComponentEffectsError> {
    if index >= 256 {
        return Err(ComponentEffectsError::Topology);
    }
    Ok(format!("{index:03}"))
}

fn parse_component_bucket_name(name: &str) -> Option<usize> {
    let index = parse_fixed_decimal(name, 6)?;
    (component_bucket_name(index).ok()?.as_str() == name).then_some(index)
}

fn parse_component_slot_name(name: &str) -> Option<usize> {
    let index = parse_fixed_decimal(name, 3)?;
    (component_slot_name(index).ok()?.as_str() == name).then_some(index)
}

fn parse_component_table_file_name(name: &str) -> Option<usize> {
    let index = parse_fixed_decimal(name.strip_suffix(".tbl")?, 6)?;
    (component_table_file_name(index).ok()?.as_str() == name).then_some(index)
}

fn parse_fixed_decimal(value: &str, width: usize) -> Option<usize> {
    (value.len() == width && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<usize>().ok())
        .flatten()
}

fn component_table_file_name(index: usize) -> Result<String, ComponentEffectsError> {
    component_table_path(index)?
        .strip_prefix("table/")
        .map(str::to_string)
        .ok_or(ComponentEffectsError::Topology)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_component_spool::ComponentTableSpool;
    use crate::managed_component_table::{
        COMPONENT_TABLE_HEADER_BYTES, ComponentIntentManifest, ComponentPriorFile,
        ComponentShardDescriptor, ComponentTableBuilder, ComponentTableRow,
        ManagedComponentArtifactKind,
    };
    use std::fs;

    #[cfg(target_os = "linux")]
    fn open_fds_beneath(root: &std::path::Path) -> usize {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(root))
            .count()
    }

    fn assert_component_lane_settled(temporary: &tempfile::TempDir) {
        let lane = temporary.path().join(".axial-publication/libraries");
        let mut lane_names = fs::read_dir(&lane)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        lane_names.sort();
        assert_eq!(
            lane_names,
            vec!["ancestors", "quarantine", "staging", "table"]
        );
        for path in [
            lane.join("table"),
            lane.join("staging"),
            lane.join("quarantine"),
            lane.join("ancestors/records"),
            lane.join("ancestors/staging"),
        ] {
            assert!(fs::read_dir(path).unwrap().next().is_none());
        }
    }

    fn single_valid_table_shard() -> Vec<u8> {
        let digest = [0x51; 20];
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Libraries, 1, [0x61; 16], [0x71; 32])
                .unwrap();
        builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 0,
                final_size: 1,
                final_sha1: digest,
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new("replacement.jar").unwrap(),
                first_created_depth: None,
                prior: Some(ComponentPriorFile {
                    size: 1,
                    sha1: digest,
                }),
            }])
            .unwrap()
            .0
    }

    async fn single_absent_row_candidate(
        temporary: &tempfile::TempDir,
    ) -> ComponentIntentCandidate {
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let root_binding = component_root_binding_sha256(lease.root()).unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let staged = b"staged-final";
        let final_sha1 = sha1::Sha1::digest(staged).into();
        let mut builder = ComponentTableBuilder::new(
            ManagedComponentKind::Libraries,
            1,
            [0x81; 16],
            root_binding,
        )
        .unwrap();
        let (encoded, descriptor) = builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 0,
                final_size: staged.len() as u64,
                final_sha1,
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new("new/library.jar").unwrap(),
                first_created_depth: Some(0),
                prior: None,
            }])
            .unwrap();
        let (manifest, _) = builder.finish().unwrap();
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();
        lane.publish_table(replay, &manifest).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        buckets.staging().write_new_exact("000", staged).unwrap();
        drop(buckets);
        lane.into_intent_candidate(lease, manifest).unwrap()
    }

    async fn single_replacement_row_candidate(
        temporary: &tempfile::TempDir,
    ) -> ComponentIntentCandidate {
        let prior = b"prior-library";
        let staged = b"replacement-library";
        let canonical = temporary.path().join("libraries/replacement.jar");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::write(&canonical, prior).unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let root_binding = component_root_binding_sha256(lease.root()).unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let mut builder = ComponentTableBuilder::new(
            ManagedComponentKind::Libraries,
            1,
            [0x83; 16],
            root_binding,
        )
        .unwrap();
        let (encoded, descriptor) = builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 0,
                final_size: staged.len() as u64,
                final_sha1: sha1::Sha1::digest(staged).into(),
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new("replacement.jar").unwrap(),
                first_created_depth: None,
                prior: Some(ComponentPriorFile {
                    size: prior.len() as u64,
                    sha1: sha1::Sha1::digest(prior).into(),
                }),
            }])
            .unwrap();
        let (manifest, _) = builder.finish().unwrap();
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();
        lane.publish_table(replay, &manifest).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        buckets.staging().write_new_exact("000", staged).unwrap();
        drop(buckets);
        lane.into_intent_candidate(lease, manifest).unwrap()
    }

    async fn two_absent_row_candidate(temporary: &tempfile::TempDir) -> ComponentIntentCandidate {
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let root_binding = component_root_binding_sha256(lease.root()).unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let staged = [b"first-staged".as_slice(), b"second-staged".as_slice()];
        let mut builder = ComponentTableBuilder::new(
            ManagedComponentKind::Libraries,
            staged.len(),
            [0x84; 16],
            root_binding,
        )
        .unwrap();
        let rows = staged
            .iter()
            .enumerate()
            .map(|(index, bytes)| ComponentTableRow {
                inventory_ordinal: index as u32,
                final_size: bytes.len() as u64,
                final_sha1: sha1::Sha1::digest(*bytes).into(),
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new(&format!("new/{index}.jar")).unwrap(),
                first_created_depth: Some(0),
                prior: None,
            })
            .collect();
        let (encoded, descriptor) = builder.push_shard(rows).unwrap();
        let (manifest, _) = builder.finish().unwrap();
        let mut spool = ComponentTableSpool::new(staged.len()).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();
        lane.publish_table(replay, &manifest).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        for (index, bytes) in staged.into_iter().enumerate() {
            buckets
                .staging()
                .write_new_exact(&component_slot_name(index).unwrap(), bytes)
                .unwrap();
        }
        drop(buckets);
        lane.into_intent_candidate(lease, manifest).unwrap()
    }

    async fn two_shard_empty_file_candidate(
        temporary: &tempfile::TempDir,
    ) -> (
        ComponentLane,
        ManagedRootPublicationLease,
        ComponentIntentManifest,
    ) {
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let root_binding = component_root_binding_sha256(lease.root()).unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let total_rows = 257_usize;
        let mut builder = ComponentTableBuilder::new(
            ManagedComponentKind::Libraries,
            total_rows,
            [0x82; 16],
            root_binding,
        )
        .unwrap();
        let mut spool = ComponentTableSpool::new(total_rows).unwrap();
        let empty_sha1 = sha1::Sha1::digest([]).into();
        for shard_index in 0..2 {
            let first = shard_index * 256;
            let count = (total_rows - first).min(256);
            let mut rows = Vec::new();
            rows.try_reserve_exact(count).unwrap();
            for index in first..first + count {
                rows.push(ComponentTableRow {
                    inventory_ordinal: index as u32,
                    final_size: 0,
                    final_sha1: empty_sha1,
                    kind: ManagedComponentArtifactKind::Library,
                    path: ArtifactRelativePath::new(&format!("artifact/{index:06}.jar")).unwrap(),
                    first_created_depth: Some(0),
                    prior: None,
                });
            }
            let (encoded, descriptor) = builder.push_shard(rows).unwrap();
            spool.append(encoded, descriptor).unwrap();
        }
        let (manifest, _) = builder.finish().unwrap();
        let replay = spool.finish(&manifest).unwrap();
        lane.publish_table(replay, &manifest).unwrap();
        for shard_index in 0..2 {
            let buckets = lane.create_shard_buckets(shard_index).unwrap();
            let first = shard_index * 256;
            let count = (total_rows - first).min(256);
            for row_index in 0..count {
                buckets
                    .staging()
                    .write_new_exact(&component_slot_name(row_index).unwrap(), b"")
                    .unwrap();
            }
        }
        (lane, lease, manifest)
    }

    #[tokio::test]
    async fn fresh_lane_has_only_the_closed_create_only_topology() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();

        assert_eq!(
            exact_entry_names(lane.lane(), MAX_COMPONENT_LANE_ENTRIES + 1).unwrap(),
            BTreeSet::from([
                COMPONENT_ANCESTORS_DIRECTORY.to_string(),
                COMPONENT_QUARANTINE_DIRECTORY.to_string(),
                COMPONENT_STAGING_DIRECTORY.to_string(),
                COMPONENT_TABLE_DIRECTORY.to_string(),
            ])
        );
        assert_eq!(lane.component(), ManagedComponentKind::Libraries);
        assert!(lane.staging().entries_bounded(1).unwrap().is_empty());
        assert!(lane.quarantine().entries_bounded(1).unwrap().is_empty());
        assert_eq!(
            exact_entry_names(&lane.ancestors, 3).unwrap(),
            BTreeSet::from([
                COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
                COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
            ])
        );
        assert!(lane.ancestor_records.entries_bounded(1).unwrap().is_empty());
        assert!(lane.ancestor_staging.entries_bounded(1).unwrap().is_empty());
        assert!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).is_ok(),
            "an exact empty fresh topology is reusable before intent"
        );
    }

    #[test]
    fn ancestor_journal_names_are_canonical_and_bounded() {
        assert_eq!(component_ancestor_bucket_name(0).unwrap(), "000000");
        assert_eq!(
            component_ancestor_record_file_name(0).unwrap(),
            "000000.anc"
        );
        assert_eq!(
            component_ancestor_bucket_name(MAX_COMPONENT_ANCESTOR_SHARDS - 1).unwrap(),
            format!("{:06}", MAX_COMPONENT_ANCESTOR_SHARDS - 1)
        );
        assert!(component_ancestor_bucket_name(MAX_COMPONENT_ANCESTOR_SHARDS).is_err());
        assert!(component_ancestor_record_file_name(MAX_COMPONENT_ANCESTOR_SHARDS).is_err());
    }

    #[tokio::test]
    async fn fresh_lane_rejects_any_preintent_ancestor_residue() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        lane.ancestor_staging.create_child_new("000000").unwrap();

        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Topology)
        ));
    }

    #[tokio::test]
    async fn fresh_lane_completes_a_partial_empty_ancestor_scaffold() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        fs::remove_dir(lane.ancestor_staging.path()).unwrap();
        drop(lane);

        let repaired =
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();

        assert!(
            repaired
                .ancestor_records
                .entries_bounded(1)
                .unwrap()
                .is_empty()
        );
        assert!(
            repaired
                .ancestor_staging
                .entries_bounded(1)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn admitted_ancestor_scaffold_replacement_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let records = lane.ancestor_records.path().to_path_buf();
        let saved_records = temporary.path().join("saved-ancestor-records");
        drop(lane);

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&records, &saved_records).unwrap();
                fs::create_dir(&records).unwrap();
            },
        );

        assert!(result.is_err());
        assert!(records.is_dir());
        assert!(saved_records.is_dir());
    }

    #[tokio::test]
    async fn fresh_lane_rejects_unknown_or_retained_preintent_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        lane.lane().write_new_exact("unexpected", b"owned").unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Topology)
        ));

        lane.lane()
            .remove_guarded_file(
                "unexpected",
                &lane
                    .lane()
                    .inspect_regular_file("unexpected")
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        lane.staging().write_new_exact("000", b"owned").unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Topology)
        ));
    }

    #[tokio::test]
    async fn unknown_residue_prevents_every_cleanup_including_temp_sweeping() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        buckets
            .staging()
            .write_new_exact(&component_slot_name(0).unwrap(), b"owned-stage")
            .unwrap();
        buckets
            .quarantine()
            .write_new_exact("sentinel", b"unknown")
            .unwrap();
        let temp_name = format!(".axial-loader-tmp-{}-1-0", std::process::id());
        fs::write(lane.table.path().join(&temp_name), b"dead-temp").unwrap();

        assert!(ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).is_err());
        assert_eq!(
            fs::read(buckets.staging().path().join("000")).unwrap(),
            b"owned-stage"
        );
        assert_eq!(
            fs::read(buckets.quarantine().path().join("sentinel")).unwrap(),
            b"unknown"
        );
        assert_eq!(
            fs::read(lane.table.path().join(temp_name)).unwrap(),
            b"dead-temp"
        );
    }

    #[tokio::test]
    async fn valid_sparse_bucket_residue_is_cleaned_and_retry_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let digest = [0x42; 20];
        let mut builder = ComponentTableBuilder::new(
            ManagedComponentKind::Libraries,
            257,
            [0x11; 16],
            [0x22; 32],
        )
        .unwrap();
        let (first_table, _) = builder
            .push_shard(
                (0..256)
                    .map(|index| ComponentTableRow {
                        inventory_ordinal: index,
                        final_size: 1,
                        final_sha1: digest,
                        kind: ManagedComponentArtifactKind::Library,
                        path: ArtifactRelativePath::new(&format!("{index:03}.jar")).unwrap(),
                        first_created_depth: None,
                        prior: Some(ComponentPriorFile {
                            size: 1,
                            sha1: digest,
                        }),
                    })
                    .collect(),
            )
            .unwrap();
        lane.table
            .write_new_exact("000000.tbl", &first_table)
            .unwrap();
        let first = lane.create_shard_buckets(0).unwrap();
        first
            .staging()
            .write_new_exact(&component_slot_name(5).unwrap(), b"stage-five")
            .unwrap();
        let dead_temp = format!(".axial-loader-tmp-{}-2-0", std::process::id());
        fs::write(first.staging().path().join(dead_temp), b"dead-temp").unwrap();
        let later = lane.create_shard_buckets(7).unwrap();
        later
            .quarantine()
            .write_new_exact(&component_slot_name(255).unwrap(), b"prior-last")
            .unwrap();
        drop((first, later, lane));

        let cleaned =
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        assert!(exact_entry_names(&cleaned.table, 1).unwrap().is_empty());
        assert!(exact_entry_names(cleaned.staging(), 1).unwrap().is_empty());
        assert!(
            exact_entry_names(cleaned.quarantine(), 1)
                .unwrap()
                .is_empty()
        );
        drop(cleaned);
        ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
    }

    #[tokio::test]
    async fn either_deterministic_park_is_recovered_before_fresh_preparation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();

        for (index, park, quarantine) in [
            (0, COMPONENT_BUCKET_PARK_A, false),
            (1, COMPONENT_BUCKET_PARK_B, true),
        ] {
            let lane =
                ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
            let buckets = lane.create_shard_buckets(index).unwrap();
            let source = if quarantine {
                buckets.quarantine().path()
            } else {
                buckets.staging().path()
            }
            .to_path_buf();
            let parent = source.parent().unwrap().to_path_buf();
            drop(buckets);
            fs::rename(&source, parent.join(park)).unwrap();
            drop(lane);

            let cleaned =
                ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
            assert!(!cleaned.staging().path().join(park).exists());
            assert!(!cleaned.quarantine().path().join(park).exists());
        }
    }

    #[tokio::test]
    async fn oversized_or_wrong_kind_slots_fail_without_cleanup_effects() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        let slot = buckets.staging().path().join("000");
        let oversized = fs::File::create(&slot).unwrap();
        oversized.set_len(MAX_TIER2_ARTIFACT_BYTES + 1).unwrap();
        drop(oversized);
        buckets
            .quarantine()
            .write_new_exact("001", b"must-remain")
            .unwrap();

        assert!(ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).is_err());
        assert_eq!(
            fs::metadata(&slot).unwrap().len(),
            MAX_TIER2_ARTIFACT_BYTES + 1
        );
        assert_eq!(
            fs::read(buckets.quarantine().path().join("001")).unwrap(),
            b"must-remain"
        );

        fs::remove_file(&slot).unwrap();
        fs::create_dir(&slot).unwrap();
        assert!(ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).is_err());
        assert!(slot.is_dir());
        assert_eq!(
            fs::read(buckets.quarantine().path().join("001")).unwrap(),
            b"must-remain"
        );
    }

    #[tokio::test]
    async fn shard_bucket_creation_is_create_only_and_exactly_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane
            .create_shard_buckets(MAX_COMPONENT_TABLE_SHARDS - 1)
            .unwrap();

        assert!(buckets.staging().path().ends_with("000781"));
        assert!(buckets.quarantine().path().ends_with("000781"));
        assert!(
            lane.create_shard_buckets(MAX_COMPONENT_TABLE_SHARDS - 1)
                .is_err()
        );
        assert!(
            lane.create_shard_buckets(MAX_COMPONENT_TABLE_SHARDS)
                .is_err()
        );
        assert_eq!(component_slot_name(0).unwrap(), "000");
        assert_eq!(component_slot_name(255).unwrap(), "255");
        assert!(component_slot_name(256).is_err());
    }

    #[tokio::test]
    async fn admitted_same_size_table_replacement_is_not_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let encoded = single_valid_table_shard();
        lane.table.write_new_exact("000000.tbl", &encoded).unwrap();
        let table_file = lane.table.path().join("000000.tbl");
        let saved_file = temporary.path().join("saved-admitted-table");
        drop(lane);
        let replacement = encoded.clone();

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&table_file, &saved_file).unwrap();
                fs::write(&table_file, &replacement).unwrap();
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(table_file).unwrap(), encoded);
        assert_eq!(fs::read(saved_file).unwrap(), encoded);
    }

    #[tokio::test]
    async fn admitted_same_size_slot_replacement_is_not_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        buckets
            .staging()
            .write_new_exact("000", b"original")
            .unwrap();
        let slot = buckets.staging().path().join("000");
        let saved_slot = temporary.path().join("saved-admitted-slot");
        drop((buckets, lane));

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&slot, &saved_slot).unwrap();
                fs::write(&slot, b"replaced").unwrap();
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(slot).unwrap(), b"replaced");
        assert_eq!(fs::read(saved_slot).unwrap(), b"original");
    }

    #[tokio::test]
    async fn admitted_same_size_temp_replacement_is_not_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let temp_name = format!(".axial-loader-tmp-{}-41-0", std::process::id());
        let temp_file = lane.table.path().join(&temp_name);
        let saved_temp = temporary.path().join("saved-admitted-temp");
        fs::write(&temp_file, b"original").unwrap();
        drop(lane);

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&temp_file, &saved_temp).unwrap();
                fs::write(&temp_file, b"replaced").unwrap();
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(temp_file).unwrap(), b"replaced");
        assert_eq!(fs::read(saved_temp).unwrap(), b"original");
    }

    #[tokio::test]
    async fn admitted_empty_bucket_replacement_is_not_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        let bucket = buckets.staging().path().to_path_buf();
        let saved_bucket = temporary.path().join("saved-admitted-bucket");
        drop((buckets, lane));

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&bucket, &saved_bucket).unwrap();
                fs::create_dir(&bucket).unwrap();
            },
        );

        assert!(result.is_err());
        assert!(bucket.is_dir());
        assert!(saved_bucket.is_dir());
    }

    #[tokio::test]
    async fn temp_added_after_admission_is_not_swept() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let buckets = lane.create_shard_buckets(0).unwrap();
        let temp_file = buckets
            .staging()
            .path()
            .join(format!(".axial-loader-tmp-{}-42-0", std::process::id()));
        drop((buckets, lane));

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || fs::write(&temp_file, b"new-temp").unwrap(),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(temp_file).unwrap(), b"new-temp");
    }

    #[tokio::test]
    async fn orphaned_lane_marker_temp_is_exactly_recovered() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let temp_file = lane
            .lane()
            .path()
            .join(format!(".axial-loader-tmp-{}-61-0", std::process::id()));
        fs::write(&temp_file, b"partial-intent").unwrap();
        drop(lane);

        let recovered =
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();

        assert!(!temp_file.exists());
        assert_eq!(
            exact_entry_names(recovered.lane(), MAX_COMPONENT_LANE_ENTRIES + 1).unwrap(),
            BTreeSet::from([
                COMPONENT_ANCESTORS_DIRECTORY.to_string(),
                COMPONENT_QUARANTINE_DIRECTORY.to_string(),
                COMPONENT_STAGING_DIRECTORY.to_string(),
                COMPONENT_TABLE_DIRECTORY.to_string(),
            ])
        );
    }

    #[tokio::test]
    async fn replaced_lane_marker_temp_is_never_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let temp_file = lane
            .lane()
            .path()
            .join(format!(".axial-loader-tmp-{}-62-0", std::process::id()));
        let saved_temp = temporary.path().join("saved-lane-marker-temp");
        fs::write(&temp_file, b"original").unwrap();
        drop(lane);

        let result = ComponentLane::prepare_fresh_with_cleanup_hook(
            &lease,
            ManagedComponentKind::Libraries,
            || {
                fs::rename(&temp_file, &saved_temp).unwrap();
                fs::write(&temp_file, b"replaced").unwrap();
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(temp_file).unwrap(), b"replaced");
        assert_eq!(fs::read(saved_temp).unwrap(), b"original");
    }

    #[tokio::test]
    async fn intent_candidate_publishes_exact_marker_last_and_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ComponentIntentCandidate>();
        assert_send::<ComponentIntentPublished>();
        assert_send::<ComponentIntentPublishFailure>();
        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let encoded = candidate.encoded_intent.clone();
        assert!(
            !candidate
                .lane
                .lane
                .path()
                .join(COMPONENT_INTENT_FILE)
                .exists()
        );

        let published = match candidate.publish_intent() {
            Ok(published) => published,
            Err(_) => panic!("publish exact component intent"),
        };

        assert_eq!(published.manifest.total_rows, 1);
        assert!(
            published
                .lane
                .lane
                .file_guard_matches(COMPONENT_INTENT_FILE, &published.intent_guard)
                .unwrap()
        );
        published.lease.revalidate().unwrap();
        assert_eq!(published.encoded_intent, encoded);
        assert_eq!(
            fs::read(published.lane.lane.path().join(COMPONENT_INTENT_FILE)).unwrap(),
            encoded
        );
    }

    #[tokio::test]
    async fn terminal_execution_commits_new_row_and_journaled_ancestors() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));

        let result = managed_component_transaction::execute_component_intent(published).await;

        assert!(matches!(result, ComponentExecutionResult::Committed(_)));
        assert_eq!(
            fs::read(temporary.path().join("libraries/new/library.jar")).unwrap(),
            b"staged-final"
        );
        assert!(
            temporary
                .path()
                .join(".axial-publication/libraries/ancestors/records/000000.anc")
                .is_file()
        );
        assert!(
            fs::read_dir(
                temporary
                    .path()
                    .join(".axial-publication/libraries/ancestors/staging/000000")
            )
            .unwrap()
            .next()
            .is_none()
        );
    }

    #[tokio::test]
    async fn terminal_execution_commits_replacement_and_retains_exact_prior() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_replacement_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));

        let result = managed_component_transaction::execute_component_intent(published).await;

        assert!(matches!(result, ComponentExecutionResult::Committed(_)));
        assert_eq!(
            fs::read(temporary.path().join("libraries/replacement.jar")).unwrap(),
            b"replacement-library"
        );
        assert_eq!(
            fs::read(
                temporary
                    .path()
                    .join(".axial-publication/libraries/quarantine/000000/000")
            )
            .unwrap(),
            b"prior-library"
        );
    }

    #[tokio::test]
    async fn terminal_execution_rolls_back_rows_and_ancestors_after_live_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));

        let result = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::AfterFirstRow,
        )
        .await;

        assert!(matches!(result, ComponentExecutionResult::RolledBack(_)));
        assert!(!temporary.path().join("libraries").exists());
        assert_eq!(
            fs::read(
                temporary
                    .path()
                    .join(".axial-publication/libraries/staging/000000/000")
            )
            .unwrap(),
            b"staged-final"
        );
        assert!(
            temporary
                .path()
                .join(".axial-publication/libraries/ancestors/staging/000000/000")
                .is_dir()
        );
    }

    #[tokio::test]
    async fn component_settlement_returns_semantic_outcome_and_same_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_replacement_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::Committed(receipt) =
            managed_component_transaction::execute_component_intent(published).await
        else {
            panic!("replacement transaction must commit")
        };
        let ComponentSettlementResult::Settled(ComponentSettledOutcome::Committed(lease)) =
            settle_component_transaction(receipt).await
        else {
            panic!("committed component must settle")
        };
        lease.revalidate().unwrap();
        assert_eq!(
            fs::read(temporary.path().join("libraries/replacement.jar")).unwrap(),
            b"replacement-library"
        );
        assert_component_lane_settled(&temporary);

        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::RolledBack(receipt) =
            managed_component_transaction::execute_component_intent_with_fault(
                published,
                managed_component_transaction::ComponentExecutionFault::AfterFirstRow,
            )
            .await
        else {
            panic!("injected execution failure must roll back")
        };
        let ComponentSettlementResult::Settled(ComponentSettledOutcome::RolledBack {
            lease,
            effect: crate::managed_component_publication::ComponentRollbackEffect::Execution,
        }) = settle_component_transaction(receipt).await
        else {
            panic!("rolled-back component must settle with its exact effect")
        };
        lease.revalidate().unwrap();
        assert!(!temporary.path().join("libraries").exists());
        assert_component_lane_settled(&temporary);
    }

    #[tokio::test]
    async fn component_settlement_retries_every_durable_frontier() {
        use managed_component_transaction::ComponentSettlementFault;

        for fault in [
            ComponentSettlementFault::AfterSettlementPromotion,
            ComponentSettlementFault::AfterStagingBucket,
            ComponentSettlementFault::AfterQuarantineBucket,
            ComponentSettlementFault::AfterTableShard,
            ComponentSettlementFault::AfterOutcomeRemoval,
            ComponentSettlementFault::AfterIntentRemoval,
            ComponentSettlementFault::AfterSettlementRemoval,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let published = single_replacement_row_candidate(&temporary)
                .await
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish component intent"));
            let ComponentExecutionResult::Committed(receipt) =
                managed_component_transaction::execute_component_intent(published).await
            else {
                panic!("frontier fixture must commit")
            };
            let ComponentSettlementResult::Retry(retry) =
                managed_component_transaction::settle_component_transaction_with_fault(
                    receipt, fault,
                )
                .await
            else {
                panic!("fault must retain settlement retry authority")
            };
            let ComponentSettlementResult::Settled(ComponentSettledOutcome::Committed(lease)) =
                retry_component_settlement(retry).await
            else {
                panic!("durable settlement frontier must resume")
            };
            lease.revalidate().unwrap();
            assert_component_lane_settled(&temporary);
        }
    }

    #[tokio::test]
    async fn component_settlement_retries_committed_and_rolled_back_ancestor_frontiers() {
        use managed_component_transaction::ComponentSettlementFault;

        for fault in [
            ComponentSettlementFault::AfterAncestorBucket,
            ComponentSettlementFault::AfterAncestorRecord,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let published = single_absent_row_candidate(&temporary)
                .await
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish component intent"));
            let ComponentExecutionResult::Committed(receipt) =
                managed_component_transaction::execute_component_intent(published).await
            else {
                panic!("committed ancestor fixture must commit")
            };
            let ComponentSettlementResult::Retry(retry) =
                managed_component_transaction::settle_component_transaction_with_fault(
                    receipt, fault,
                )
                .await
            else {
                panic!("committed ancestor frontier must be retryable")
            };
            assert!(matches!(
                retry_component_settlement(retry).await,
                ComponentSettlementResult::Settled(ComponentSettledOutcome::Committed(_))
            ));

            let temporary = tempfile::tempdir().unwrap();
            let published = single_absent_row_candidate(&temporary)
                .await
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish component intent"));
            let ComponentExecutionResult::RolledBack(receipt) =
                managed_component_transaction::execute_component_intent_with_fault(
                    published,
                    managed_component_transaction::ComponentExecutionFault::AfterFirstRow,
                )
                .await
            else {
                panic!("rolled-back ancestor fixture must roll back")
            };
            let ComponentSettlementResult::Retry(retry) =
                managed_component_transaction::settle_component_transaction_with_fault(
                    receipt, fault,
                )
                .await
            else {
                panic!("rolled-back ancestor frontier must be retryable")
            };
            assert!(matches!(
                retry_component_settlement(retry).await,
                ComponentSettlementResult::Settled(ComponentSettledOutcome::RolledBack { .. })
            ));
            assert_component_lane_settled(&temporary);
        }
    }

    #[tokio::test]
    async fn startup_resumes_each_settlement_marker_suffix() {
        use managed_component_transaction::ComponentSettlementFault;

        for fault in [
            ComponentSettlementFault::AfterSettlementPromotion,
            ComponentSettlementFault::AfterOutcomeRemoval,
            ComponentSettlementFault::AfterIntentRemoval,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let published = single_absent_row_candidate(&temporary)
                .await
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish component intent"));
            let ComponentExecutionResult::Committed(receipt) =
                managed_component_transaction::execute_component_intent(published).await
            else {
                panic!("startup settlement fixture must commit")
            };
            let ComponentSettlementResult::Retry(retry) =
                managed_component_transaction::settle_component_transaction_with_fault(
                    receipt, fault,
                )
                .await
            else {
                panic!("fault must retain settlement retry authority")
            };
            drop(retry);
            let lease = ManagedRootPublicationLease::acquire(
                ManagedDir::open_root(temporary.path()).unwrap(),
            )
            .await
            .unwrap();
            let ComponentStartupRecoveryResult::Settled(ComponentSettledOutcome::Committed(lease)) =
                recover_component_transaction(lease, ManagedComponentKind::Libraries).await
            else {
                panic!("startup must resume the embedded settlement")
            };
            lease.revalidate().unwrap();
            assert_component_lane_settled(&temporary);
        }
    }

    #[tokio::test]
    async fn startup_rejects_outcome_only_settlement_suffix_without_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::Committed(receipt) =
            managed_component_transaction::execute_component_intent(published).await
        else {
            panic!("invalid suffix fixture must commit")
        };
        let ComponentSettlementResult::Retry(retry) =
            managed_component_transaction::settle_component_transaction_with_fault(
                receipt,
                managed_component_transaction::ComponentSettlementFault::AfterSettlementPromotion,
            )
            .await
        else {
            panic!("settlement marker fault must be retryable")
        };
        let lane = temporary.path().join(".axial-publication/libraries");
        fs::remove_file(lane.join("intent.bin")).unwrap();
        drop(retry);
        let lease =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(temporary.path()).unwrap())
                .await
                .unwrap();

        assert!(matches!(
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
            ComponentStartupRecoveryResult::Transaction(
                ComponentExecutionResult::RecoveryRequired(_)
            )
        ));
        assert!(lane.join("outcome.bin").is_file());
        assert!(lane.join("settlement.bin").is_file());
        assert!(lane.join("ancestors/records/000000.anc").is_file());
    }

    #[tokio::test]
    async fn settlement_admission_rejects_foreign_rows_before_ancestor_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::Committed(receipt) =
            managed_component_transaction::execute_component_intent(published).await
        else {
            panic!("foreign obstruction fixture must commit")
        };
        let ComponentSettlementResult::Retry(retry) =
            managed_component_transaction::settle_component_transaction_with_fault(
                receipt,
                managed_component_transaction::ComponentSettlementFault::AfterSettlementPromotion,
            )
            .await
        else {
            panic!("settlement marker fault must be retryable")
        };
        let lane = temporary.path().join(".axial-publication/libraries");
        let foreign = lane.join("staging/000000/foreign");
        fs::write(&foreign, b"foreign").unwrap();
        let record = lane.join("ancestors/records/000000.anc");
        let record_bytes = fs::read(&record).unwrap();

        let ComponentSettlementResult::Retry(retry) = retry_component_settlement(retry).await
        else {
            panic!("foreign row entry must fail closed")
        };
        assert_eq!(fs::read(&record).unwrap(), record_bytes);
        assert!(lane.join("ancestors/staging/000000").is_dir());
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign");

        fs::remove_file(foreign).unwrap();
        assert!(matches!(
            retry_component_settlement(retry).await,
            ComponentSettlementResult::Settled(ComponentSettledOutcome::Committed(_))
        ));
        assert_component_lane_settled(&temporary);
    }

    #[tokio::test]
    async fn settlement_rejects_replaced_marker_and_parked_ancestor_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::Committed(receipt) =
            managed_component_transaction::execute_component_intent(published).await
        else {
            panic!("marker replacement fixture must commit")
        };
        let ComponentSettlementResult::Retry(retry) =
            managed_component_transaction::settle_component_transaction_with_fault(
                receipt,
                managed_component_transaction::ComponentSettlementFault::AfterSettlementPromotion,
            )
            .await
        else {
            panic!("settlement marker fault must be retryable")
        };
        let settlement = temporary
            .path()
            .join(".axial-publication/libraries/settlement.bin");
        let saved = temporary.path().join("saved-settlement");
        let bytes = fs::read(&settlement).unwrap();
        fs::rename(&settlement, &saved).unwrap();
        fs::write(&settlement, bytes).unwrap();
        let ComponentSettlementResult::Retry(_retry) = retry_component_settlement(retry).await
        else {
            panic!("same-byte settlement replacement must fail closed")
        };
        assert!(settlement.is_file());
        assert!(saved.is_file());

        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let rolled_back = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::AfterFirstRow,
        )
        .await;
        let ComponentExecutionResult::RolledBack(receipt) = rolled_back else {
            panic!("park replacement fixture must roll back")
        };
        let ComponentSettlementResult::Retry(retry) =
            managed_component_transaction::settle_component_transaction_with_fault(
                receipt,
                managed_component_transaction::ComponentSettlementFault::AfterSettlementPromotion,
            )
            .await
        else {
            panic!("settlement marker fault must be retryable")
        };
        let bucket = temporary
            .path()
            .join(".axial-publication/libraries/ancestors/staging/000000");
        let mut slots = fs::read_dir(&bucket)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        slots.sort();
        let last = slots.pop().unwrap();
        let saved = temporary.path().join("saved-ancestor-slot");
        fs::rename(bucket.join(last), &saved).unwrap();
        fs::create_dir(bucket.join("slot-park-a")).unwrap();
        let ComponentSettlementResult::Retry(_retry) = retry_component_settlement(retry).await
        else {
            panic!("foreign parked ancestor identity must fail closed")
        };
        assert!(bucket.join("slot-park-a").is_dir());
        assert!(saved.is_dir());
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_abandon_component_settlement() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_replacement_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentExecutionResult::Committed(receipt) =
            managed_component_transaction::execute_component_intent(published).await
        else {
            panic!("cancellation fixture must commit")
        };
        let caller = tokio::spawn(settle_component_transaction(receipt));
        tokio::task::yield_now().await;
        caller.abort();

        let settlement = temporary
            .path()
            .join(".axial-publication/libraries/settlement.bin");
        for _ in 0..200 {
            if !settlement.exists()
                && temporary
                    .path()
                    .join(".axial-publication/libraries/table")
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_none())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_component_lane_settled(&temporary);
        assert_eq!(
            fs::read(temporary.path().join("libraries/replacement.jar")).unwrap(),
            b"replacement-library"
        );
    }

    #[tokio::test]
    async fn attempted_outcome_publication_retains_recovery_guard() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));

        let result = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::OutcomePromotionAttempted,
        )
        .await;

        let ComponentExecutionResult::RecoveryRequired(_) = result else {
            panic!("attempted outcome must retain recovery authority");
        };
        assert!(
            temporary
                .path()
                .join(".axial-publication/libraries/outcome.bin")
                .is_file()
        );
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_cancel_terminal_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let caller = tokio::spawn(managed_component_transaction::execute_component_intent(
            published,
        ));
        tokio::task::yield_now().await;
        caller.abort();

        let outcome = temporary
            .path()
            .join(".axial-publication/libraries/outcome.bin");
        for _ in 0..200 {
            if outcome.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(outcome.is_file());
        assert_eq!(
            fs::read(temporary.path().join("libraries/new/library.jar")).unwrap(),
            b"staged-final"
        );
    }

    #[tokio::test]
    async fn startup_recovery_returns_lease_when_no_transaction_exists() {
        let temporary = tempfile::tempdir().unwrap();
        let lease =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(temporary.path()).unwrap())
                .await
                .unwrap();

        let ComponentStartupRecoveryResult::NoTransaction(lease) =
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await
        else {
            panic!("marker-free root must not synthesize a transaction")
        };

        lease.revalidate().unwrap();

        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        fs::remove_dir(lane.ancestor_staging.path()).unwrap();
        drop(lane);
        let ComponentStartupRecoveryResult::NoTransaction(lease) =
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await
        else {
            panic!("marker-free partial scaffold must use pre-intent recovery")
        };
        assert!(
            lease
                .publication_directory()
                .path()
                .join("libraries/ancestors/staging")
                .is_dir()
        );
    }

    #[tokio::test]
    async fn startup_recovery_rejects_outcome_without_intent_and_any_settlement() {
        for marker in [
            crate::managed_component_publication::COMPONENT_OUTCOME_FILE,
            crate::managed_component_publication::COMPONENT_SETTLEMENT_FILE,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let lease = ManagedRootPublicationLease::acquire(
                ManagedDir::open_root(temporary.path()).unwrap(),
            )
            .await
            .unwrap();
            let lane =
                ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
            lane.lane
                .write_new_exact(marker, b"attempted-marker")
                .unwrap();
            drop(lane);

            assert!(matches!(
                recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
                ComponentStartupRecoveryResult::Transaction(
                    ComponentExecutionResult::RecoveryRequired(_)
                )
            ));
        }
    }

    #[tokio::test]
    async fn restart_recovery_rolls_back_pristine_and_partial_new_rows() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentIntentPublished { lease, .. } = published;
        assert!(matches!(
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert!(!temporary.path().join("libraries").exists());

        let temporary = tempfile::tempdir().unwrap();
        let published = two_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashAfterFirstRow,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("injected row crash must retain recovery authority")
        };
        assert!(temporary.path().join("libraries/new/0.jar").is_file());
        let (lease, component) = recovery.into_restart_seed();
        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert!(!temporary.path().join("libraries").exists());
        assert_eq!(
            fs::read(
                temporary
                    .path()
                    .join(".axial-publication/libraries/staging/000000/000")
            )
            .unwrap(),
            b"first-staged"
        );
    }

    #[tokio::test]
    async fn restart_recovery_reverses_partial_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_replacement_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashAfterFirstReplacementQuarantine,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("injected replacement crash must retain recovery authority")
        };
        assert!(!temporary.path().join("libraries/replacement.jar").exists());
        let (lease, component) = recovery.into_restart_seed();

        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert_eq!(
            fs::read(temporary.path().join("libraries/replacement.jar")).unwrap(),
            b"prior-library"
        );

        let temporary = tempfile::tempdir().unwrap();
        let published = single_replacement_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashAfterFirstReplacementQuarantine,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("injected replacement crash must retain recovery authority")
        };
        let (lease, component) = recovery.into_restart_seed();
        fs::remove_file(
            temporary
                .path()
                .join(".axial-publication/libraries/intent.bin"),
        )
        .unwrap();
        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(
                ComponentExecutionResult::RecoveryRequired(_)
            )
        ));
        assert_eq!(
            fs::read(
                temporary
                    .path()
                    .join(".axial-publication/libraries/quarantine/000000/000")
            )
            .unwrap(),
            b"prior-library"
        );
    }

    #[tokio::test]
    async fn restart_recovery_completes_committed_intent_and_replays_outcomes() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashBeforeOutcome,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("pre-outcome crash must retain recovery authority")
        };
        let (lease, component) = recovery.into_restart_seed();
        let completed = recover_component_transaction(lease, component).await;
        let ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::Committed(
            receipt,
        )) = completed
        else {
            panic!("fully committed intent must publish its outcome")
        };
        let (lease, component) = receipt.into_restart_seed();
        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::Committed(_))
        ));

        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let rolled_back = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::AfterFirstRow,
        )
        .await;
        let ComponentExecutionResult::RolledBack(receipt) = rolled_back else {
            panic!("live failure must publish rolled-back outcome")
        };
        let (lease, component) = receipt.into_restart_seed();
        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
    }

    #[tokio::test]
    async fn restart_recovery_rolls_back_partial_ancestor_prefix_and_rejects_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashAfterFirstAncestor,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("ancestor crash must retain recovery authority")
        };
        let (lease, component) = recovery.into_restart_seed();
        assert!(matches!(
            recover_component_transaction(lease, component).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert!(!temporary.path().join("libraries").exists());

        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let crashed = managed_component_transaction::execute_component_intent_with_fault(
            published,
            managed_component_transaction::ComponentExecutionFault::CrashAfterFirstAncestor,
        )
        .await;
        let ComponentExecutionResult::RecoveryRequired(recovery) = crashed else {
            panic!("ancestor crash must retain recovery authority")
        };
        let (lease, component) = recovery.into_restart_seed();
        let slot = temporary
            .path()
            .join(".axial-publication/libraries/ancestors/staging/000000/001");
        let saved = temporary.path().join("saved-journaled-ancestor");
        fs::rename(&slot, &saved).unwrap();
        fs::create_dir(&slot).unwrap();
        let failed = recover_component_transaction(lease, component).await;
        let ComponentStartupRecoveryResult::Transaction(
            ComponentExecutionResult::RecoveryRequired(recovery),
        ) = failed
        else {
            panic!("replaced journaled ancestor identity must fail closed")
        };
        assert!(matches!(
            retry_component_recovery(recovery).await,
            ComponentRecoveryRetryResult::Transaction(ComponentExecutionResult::RecoveryRequired(
                _
            ))
        ));
        assert!(slot.is_dir());
        assert!(saved.is_dir());
    }

    #[tokio::test]
    async fn restart_recovery_cleans_only_exact_empty_unjournaled_ancestor_prefix() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let bucket = published
            .lane
            .ancestor_staging
            .create_child_new("000000")
            .unwrap();
        bucket.create_child_new("000").unwrap();
        let bucket_path = bucket.path().to_path_buf();
        drop(bucket);
        let ComponentIntentPublished { lease, .. } = published;
        assert!(matches!(
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert!(!bucket_path.exists());

        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let bucket = published
            .lane
            .ancestor_staging
            .create_child_new("000000")
            .unwrap();
        let slot = bucket.create_child_new("000").unwrap();
        slot.write_new_exact("foreign", b"do-not-delete").unwrap();
        let slot_path = slot.path().to_path_buf();
        drop((slot, bucket));
        let ComponentIntentPublished { lease, .. } = published;
        assert!(matches!(
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
            ComponentStartupRecoveryResult::Transaction(
                ComponentExecutionResult::RecoveryRequired(_)
            )
        ));
        assert_eq!(
            fs::read(slot_path.join("foreign")).unwrap(),
            b"do-not-delete"
        );

        for park_slot in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let published = single_absent_row_candidate(&temporary)
                .await
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish component intent"));
            let bucket = published
                .lane
                .ancestor_staging
                .create_child_new("000000")
                .unwrap();
            if park_slot {
                bucket.create_child_new("000").unwrap();
                fs::rename(bucket.path().join("000"), bucket.path().join("slot-park-a")).unwrap();
                drop(bucket);
            } else {
                let bucket_path = bucket.path().to_path_buf();
                drop(bucket);
                fs::rename(
                    &bucket_path,
                    published
                        .lane
                        .ancestor_staging
                        .path()
                        .join(COMPONENT_BUCKET_PARK_A),
                )
                .unwrap();
            }
            let ComponentIntentPublished { lease, .. } = published;
            assert!(matches!(
                recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
                ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(
                    _
                ))
            ));
        }
    }

    #[tokio::test]
    async fn restart_recovery_cleans_authenticated_orphan_marker_temps() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let lane_temp = format!(".axial-loader-tmp-{}-900001-0", std::process::id());
        let record_temp = format!(".axial-loader-tmp-{}-900002-0", std::process::id());
        let lane_temp_path = published.lane.lane.path().join(&lane_temp);
        let record_temp_path = published.lane.ancestor_records.path().join(&record_temp);
        fs::write(&lane_temp_path, b"orphan-intent-temp").unwrap();
        fs::write(&record_temp_path, b"orphan-record-temp").unwrap();
        let ComponentIntentPublished { lease, .. } = published;

        assert!(matches!(
            recover_component_transaction(lease, ManagedComponentKind::Libraries).await,
            ComponentStartupRecoveryResult::Transaction(ComponentExecutionResult::RolledBack(_))
        ));
        assert!(!lane_temp_path.exists());
        assert!(!record_temp_path.exists());
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_abandon_restart_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let published = single_absent_row_candidate(&temporary)
            .await
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish component intent"));
        let ComponentIntentPublished { lease, .. } = published;
        let caller = tokio::spawn(recover_component_transaction(
            lease,
            ManagedComponentKind::Libraries,
        ));
        tokio::task::yield_now().await;
        caller.abort();

        let outcome = temporary
            .path()
            .join(".axial-publication/libraries/outcome.bin");
        for _ in 0..200 {
            if outcome.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(outcome.is_file());
        assert!(!temporary.path().join("libraries").exists());
    }

    #[tokio::test]
    async fn intent_publication_distinguishes_before_and_attempted_faults() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let lane_path = candidate.lane.lane.path().to_path_buf();
        let before = match candidate
            .publish_intent_with_fault(ComponentIntentPublishFault::BeforeMarkerPromotion)
        {
            Err(failure) => failure,
            Ok(_) => panic!("injected pre-promotion failure was ignored"),
        };
        assert!(matches!(
            &before,
            ComponentIntentPublishFailure::BeforePromotion { .. }
        ));
        assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());
        drop(before);

        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let lane_path = candidate.lane.lane.path().to_path_buf();
        let attempted = match candidate
            .publish_intent_with_fault(ComponentIntentPublishFault::PromotionAttemptedWithoutMarker)
        {
            Err(failure) => failure,
            Ok(_) => panic!("injected attempted promotion without a marker was ignored"),
        };
        let recovered = recover_component_intent_publication(attempted)
            .await
            .unwrap_or_else(|_| panic!("attempted promotion is semantically recoverable"));
        let ComponentIntentPublicationRecovery::Retry(candidate) = recovered else {
            panic!("exact marker absence must return the retained retry candidate")
        };
        assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());
        assert!(candidate.publish_intent().is_ok());

        for fault in [
            ComponentIntentPublishFault::AfterMarkerPromotion,
            ComponentIntentPublishFault::AfterLaneSynced,
            ComponentIntentPublishFault::AfterPublicationSynced,
            ComponentIntentPublishFault::AfterRootSynced,
            ComponentIntentPublishFault::AfterLeaseRevalidated,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let candidate = single_absent_row_candidate(&temporary).await;
            let lane_path = candidate.lane.lane.path().to_path_buf();
            let attempted = match candidate.publish_intent_with_fault(fault) {
                Err(failure) => failure,
                Ok(_) => panic!("injected post-promotion failure was ignored"),
            };
            assert!(matches!(
                &attempted,
                ComponentIntentPublishFailure::PromotionAttempted { .. }
            ));
            assert!(lane_path.join(COMPONENT_INTENT_FILE).is_file());
            assert!(matches!(
                recover_component_intent_publication(attempted)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("attempted marker promotion enters semantic recovery")
                    }),
                ComponentIntentPublicationRecovery::Transaction(
                    ComponentExecutionResult::RolledBack(_)
                )
            ));
        }

        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let intent = candidate.lane.lane.path().join(COMPONENT_INTENT_FILE);
        let saved = temporary.path().join("saved-attempted-intent");
        let attempted = match candidate
            .publish_intent_with_fault(ComponentIntentPublishFault::AfterMarkerPromotion)
        {
            Err(failure) => failure,
            Ok(_) => panic!("injected attempted marker promotion was ignored"),
        };
        fs::rename(&intent, &saved).unwrap();
        fs::write(&intent, b"replacement").unwrap();
        assert!(matches!(
            recover_component_intent_publication(attempted)
                .await
                .unwrap_or_else(|_| panic!("attempted publication remains semantically owned")),
            ComponentIntentPublicationRecovery::Transaction(
                ComponentExecutionResult::RecoveryRequired(_)
            )
        ));
        assert_eq!(fs::read(intent).unwrap(), b"replacement");
        assert!(saved.is_file());
    }

    #[tokio::test]
    async fn candidate_rejects_same_byte_table_and_stage_replacements_before_marker() {
        for replace_table in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let candidate = single_absent_row_candidate(&temporary).await;
            let target = if replace_table {
                candidate.lane.table.path().join("000000.tbl")
            } else {
                candidate.lane.staging.path().join("000000/000")
            };
            let saved = temporary.path().join(if replace_table {
                "saved-table-file"
            } else {
                "saved-stage-file"
            });
            let bytes = fs::read(&target).unwrap();
            fs::rename(&target, &saved).unwrap();
            fs::write(&target, &bytes).unwrap();
            let lane_path = candidate.lane.lane.path().to_path_buf();

            let failure = match candidate.publish_intent() {
                Err(failure) => failure,
                Ok(_) => panic!("identity replacement did not invalidate candidate"),
            };

            assert!(matches!(
                failure,
                ComponentIntentPublishFailure::BeforePromotion { .. }
            ));
            assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());
            assert_eq!(fs::read(target).unwrap(), bytes);
            assert_eq!(fs::read(saved).unwrap(), bytes);
        }
    }

    #[tokio::test]
    async fn candidate_rejects_replaced_ancestor_scaffold_before_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let records = candidate.lane.ancestor_records.path().to_path_buf();
        let lane_path = candidate.lane.lane.path().to_path_buf();
        let saved_records = temporary.path().join("saved-candidate-ancestor-records");
        fs::rename(&records, &saved_records).unwrap();
        fs::create_dir(&records).unwrap();

        let failure = match candidate.publish_intent() {
            Err(failure) => failure,
            Ok(_) => panic!("replaced ancestor records directory was accepted"),
        };

        assert!(matches!(
            failure,
            ComponentIntentPublishFailure::BeforePromotion { .. }
        ));
        assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());
    }

    #[tokio::test]
    async fn candidate_rejects_canonical_sentinel_and_child_directory_drift() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        fs::create_dir_all(temporary.path().join("libraries/new")).unwrap();
        fs::write(
            temporary.path().join("libraries/new/library.jar"),
            b"canonical-drift",
        )
        .unwrap();
        let lane_path = candidate.lane.lane.path().to_path_buf();
        let failure = match candidate.publish_intent() {
            Err(failure) => failure,
            Ok(_) => panic!("canonical absence sentinel drift was accepted"),
        };
        assert!(matches!(
            &failure,
            ComponentIntentPublishFailure::BeforePromotion { .. }
        ));
        assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());

        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let table = candidate.lane.table.path().to_path_buf();
        let saved_table = temporary.path().join("saved-table-directory");
        let encoded = fs::read(table.join("000000.tbl")).unwrap();
        fs::rename(&table, &saved_table).unwrap();
        fs::create_dir(&table).unwrap();
        fs::write(table.join("000000.tbl"), encoded).unwrap();
        let lane_path = candidate.lane.lane.path().to_path_buf();
        let failure = match candidate.publish_intent() {
            Err(failure) => failure,
            Ok(_) => panic!("table directory identity drift was accepted"),
        };
        assert!(matches!(
            &failure,
            ComponentIntentPublishFailure::BeforePromotion { .. }
        ));
        assert!(!lane_path.join(COMPONENT_INTENT_FILE).exists());
        assert!(table.is_dir());
        assert!(saved_table.is_dir());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn candidate_rejects_root_binding_replacement_before_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = single_absent_row_candidate(&temporary).await;
        let original_root = temporary.path().to_path_buf();
        let saved_root = original_root.with_extension("saved-component-root");
        fs::rename(&original_root, &saved_root).unwrap();
        fs::create_dir(&original_root).unwrap();

        let failure = match candidate.publish_intent() {
            Err(failure) => failure,
            Ok(_) => panic!("root identity replacement was accepted"),
        };

        assert!(matches!(
            failure,
            ComponentIntentPublishFailure::BeforePromotion { .. }
        ));
        assert!(
            !original_root
                .join(".axial-publication/libraries/intent.bin")
                .exists()
        );
        drop(failure);
        fs::remove_dir(&original_root).unwrap();
        fs::rename(&saved_root, &original_root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn streamed_candidate_authority_has_constant_fd_growth_across_shards() {
        let temporary = tempfile::tempdir().unwrap();
        let (lane, lease, manifest) = two_shard_empty_file_candidate(&temporary).await;
        let before = open_fds_beneath(temporary.path());

        let candidate = lane.into_intent_candidate(lease, manifest).unwrap();

        assert!(open_fds_beneath(temporary.path()) <= before + 1);
        assert_eq!(candidate.authority.shards.len(), 2);
        assert_eq!(
            candidate
                .authority
                .shards
                .iter()
                .map(|shard| shard.rows.len())
                .sum::<usize>(),
            257
        );
        // The 782-shard bound repeats this same one-shard-at-a-time handle lifetime.
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn maximum_bucket_admission_does_not_retain_per_bucket_handles() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = ManagedDir::open_root(temporary.path()).unwrap();
        for index in 0..MAX_COMPONENT_TABLE_SHARDS {
            fs::create_dir(temporary.path().join(component_bucket_name(index).unwrap())).unwrap();
        }
        let temp_name = format!(".axial-loader-tmp-{}-1-0", std::process::id());
        fs::write(
            temporary
                .path()
                .join(component_bucket_name(MAX_COMPONENT_TABLE_SHARDS - 1).unwrap())
                .join(temp_name),
            b"dead-temp",
        )
        .unwrap();
        let before = open_fds_beneath(temporary.path());

        let plan = ComponentBucketCleanupPlan::admit(directory).unwrap();

        assert_eq!(plan.buckets.len(), MAX_COMPONENT_TABLE_SHARDS);
        assert!(open_fds_beneath(temporary.path()) <= before + 2);
    }

    #[test]
    fn canonical_walk_reports_exact_missing_depth_and_observes_a_stable_file() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/library.jar"),
            b"authenticated-library",
        )
        .unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let existing = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
        )
        .unwrap();
        assert_eq!(existing.first_created_depth(), None);
        assert_eq!(existing.file_name(), "library.jar");
        assert!(existing.parent().is_some());
        assert!(existing.remaining_parent_segments().is_empty());
        let ComponentCanonicalObservation::Regular(observed) = existing.observe().unwrap() else {
            panic!("existing regular file was not observed")
        };
        assert_eq!(observed.size(), 21);
        assert_ne!(observed.sha1(), [0; 20]);
        assert_eq!(observed.file_name(), "library.jar");
        assert!(
            observed
                .parent()
                .file_guard_matches(observed.file_name(), observed.guard())
                .unwrap()
        );

        let missing_parent = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/missing/library.jar").unwrap(),
        )
        .unwrap();
        assert_eq!(missing_parent.first_created_depth(), Some(2));
        assert!(missing_parent.parent().is_none());
        assert_eq!(
            missing_parent.remaining_parent_segments(),
            &["missing".to_string()]
        );
        let stable_org = root
            .open_child("libraries")
            .unwrap()
            .open_child("org")
            .unwrap();
        assert_eq!(
            missing_parent.creation_anchor().identity().unwrap(),
            stable_org.identity().unwrap()
        );
        assert!(matches!(
            missing_parent.observe().unwrap(),
            ComponentCanonicalObservation::Absent
        ));
        fs::create_dir(temporary.path().join("libraries/org/missing")).unwrap();
        assert!(
            missing_parent.observe().is_err(),
            "a created ancestor must invalidate the recorded missing depth"
        );

        let missing_root = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Assets,
            &ArtifactRelativePath::new("indexes/current.json").unwrap(),
        )
        .unwrap();
        assert_eq!(missing_root.first_created_depth(), Some(0));
        assert_eq!(
            missing_root.remaining_parent_segments(),
            &["assets".to_string(), "indexes".to_string()]
        );
        assert_eq!(
            missing_root.creation_anchor().identity().unwrap(),
            root.identity().unwrap()
        );
    }

    #[test]
    fn canonical_walk_rejects_portable_ancestor_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/Org/example")).unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        assert!(
            plan_component_canonical_path(
                &root,
                ManagedComponentKind::Libraries,
                &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_observation_rechecks_portable_leaf_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let plan = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
        )
        .unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/Library.jar"),
            b"portable-alias",
        )
        .unwrap();

        assert!(plan.observe().is_err());
    }

    #[tokio::test]
    async fn table_publication_replays_create_new_and_parses_the_durable_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let digest = [0x55; 20];
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Libraries, 1, [0x11; 16], [0x22; 32])
                .unwrap();
        let (encoded, descriptor) = builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 0,
                final_size: 7,
                final_sha1: digest,
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new("example/library.jar").unwrap(),
                first_created_depth: None,
                prior: Some(ComponentPriorFile {
                    size: 7,
                    sha1: digest,
                }),
            }])
            .unwrap();
        let (manifest, expected_summary) = builder.finish().unwrap();
        let mut invalid_manifest = manifest.clone();
        invalid_manifest.total_rows += 1;
        let mut invalid_spool = ComponentTableSpool::new(1).unwrap();
        invalid_spool
            .append(encoded.clone(), descriptor.clone())
            .unwrap();
        let invalid_replay = invalid_spool.finish(&manifest).unwrap();
        assert!(
            lane.publish_table(invalid_replay, &invalid_manifest)
                .is_err()
        );
        assert!(lane.table.entries_bounded(1).unwrap().is_empty());

        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();

        let durable_summary = lane.publish_table(replay, &manifest).unwrap();
        assert_eq!(durable_summary, expected_summary);
        drop((durable_summary, lane));

        let cleaned =
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        assert!(cleaned.table.entries_bounded(1).unwrap().is_empty());
        cleaned
            .table
            .write_new_exact("000000.tbl", b"not-an-owned-table")
            .unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Table(_))
        ));
    }

    #[tokio::test]
    async fn invalid_new_table_shard_is_guarded_removed_before_returning() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let encoded = vec![0_u8; COMPONENT_TABLE_HEADER_BYTES];
        let descriptor = ComponentShardDescriptor {
            shard_index: 0,
            first_row: 0,
            row_count: 1,
            byte_len: u32::try_from(encoded.len()).unwrap(),
            final_bytes: 1,
            prior_bytes: 0,
            sha256: Sha256::digest(&encoded).into(),
        };
        let manifest = ComponentIntentManifest {
            component: ManagedComponentKind::Libraries,
            total_rows: 1,
            final_bytes: 1,
            prior_bytes: 0,
            transaction_nonce: [0x11; 16],
            root_binding_sha256: [0x22; 32],
            logical_rows_sha256: [0x33; 32],
            projection_sha256: [0x44; 32],
            shards: vec![descriptor.clone()],
        };
        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();

        assert!(lane.publish_table(replay, &manifest).is_err());
        assert!(exact_entry_names(&lane.table, 1).unwrap().is_empty());
    }
}
