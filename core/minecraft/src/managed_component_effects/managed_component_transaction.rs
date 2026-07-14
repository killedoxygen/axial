use super::*;
use crate::managed_component_ancestor_journal::{
    COMPONENT_ANCESTOR_RECORDS_PER_SHARD, ComponentAncestorJournalAuthority,
    ComponentAncestorJournalRecord, MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES,
};
use crate::managed_component_publication::{
    COMPONENT_OUTCOME_BYTES, COMPONENT_OUTCOME_FILE, COMPONENT_SETTLEMENT_FILE,
    ComponentObservedCanonical, ComponentOutcomeRecord, ComponentRecoveryDecision,
    ComponentRecoveryEntryState, ComponentRecoveryObservation, ComponentRecoveryPlan,
    ComponentRecoveryPlanner, ComponentRollbackEffect, ComponentTerminalOutcome,
    decode_component_outcome, encode_component_outcome,
};
use crate::managed_component_table::{
    ComponentCreatedAncestor, ComponentTableParser, ComponentTableRow, ComponentTableShard,
    MAX_COMPONENT_TABLE_SHARD_BYTES, decode_component_intent_manifest,
    decode_component_table_shard,
};
use crate::managed_fs::{
    ManagedCreateOnlyWriteFailure, ManagedDirectoryMoveFailure, ManagedFileGuard,
};
use crate::managed_publication::run_publication_blocking;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

const ANCESTOR_SLOT_PARK_A: &str = "slot-park-a";
const ANCESTOR_SLOT_PARK_B: &str = "slot-park-b";

pub(crate) enum ComponentExecutionResult {
    Committed(ComponentCommitReceipt),
    RolledBack(ComponentRollbackReceipt),
    RecoveryRequired(ComponentRecoveryRequired),
}

pub(crate) enum ComponentStartupRecoveryResult {
    NoTransaction(ManagedRootPublicationLease),
    Transaction(ComponentExecutionResult),
}

pub(crate) enum ComponentIntentPublicationRecovery {
    Retry(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

pub(crate) enum ComponentRecoveryRetryResult {
    NoTransaction(ManagedRootPublicationLease),
    RetryIntent(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

pub(crate) struct ComponentCommitReceipt {
    context: ComponentIntentPublished,
    outcome_guard: ManagedFileGuard,
}

pub(crate) struct ComponentRollbackReceipt {
    context: ComponentIntentPublished,
    outcome_guard: ManagedFileGuard,
}

pub(crate) struct ComponentRecoveryRequired {
    authority: ComponentRecoveryAuthority,
}

#[cfg(test)]
impl ComponentCommitReceipt {
    pub(super) fn into_restart_seed(self) -> (ManagedRootPublicationLease, ManagedComponentKind) {
        let component = self.context.manifest.component;
        drop(self.outcome_guard);
        (self.context.lease, component)
    }
}

#[cfg(test)]
impl ComponentRollbackReceipt {
    pub(super) fn into_restart_seed(self) -> (ManagedRootPublicationLease, ManagedComponentKind) {
        let component = self.context.manifest.component;
        drop(self.outcome_guard);
        (self.context.lease, component)
    }
}

#[cfg(test)]
impl ComponentRecoveryRequired {
    pub(super) fn into_restart_seed(self) -> (ManagedRootPublicationLease, ManagedComponentKind) {
        let ComponentRecoveryAuthority::Published { context, .. } = self.authority else {
            panic!("test recovery authority was not a published intent")
        };
        (context.lease, context.manifest.component)
    }
}

enum ComponentRecoveryAuthority {
    Published {
        context: ComponentIntentPublished,
        outcome_guard: Option<ManagedFileGuard>,
    },
    Restart {
        lease: ManagedRootPublicationLease,
        component: ManagedComponentKind,
    },
    IntentPromotionAttempted(ComponentIntentPublishFailure),
}

struct ComponentRestartAdmission {
    lane: ComponentLane,
    manifest: ComponentIntentManifest,
    encoded_intent: Vec<u8>,
    intent_guard: ManagedFileGuard,
    outcome_guard: Option<ManagedFileGuard>,
}

#[derive(Clone, Copy)]
struct ComponentTransactionError;

enum OutcomePublicationFailure {
    BeforePromotion,
    PromotionAttempted(Option<ManagedFileGuard>),
}

enum BlockingDisposition {
    NoTransaction,
    RetryIntent,
    Committed(ManagedFileGuard),
    RolledBack(ManagedFileGuard),
    RecoveryRequired(Option<ManagedFileGuard>),
}

enum RecoveryOwnerResult {
    NoTransaction(ManagedRootPublicationLease),
    RetryIntent(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

enum RecoveryNormalization {
    Published,
    NoTransaction,
    RetryIntent,
}

struct AncestorRecoveryPlan {
    durable_shards: usize,
    canonical_records: usize,
}

struct EmptyRecoveryPark {
    name: &'static str,
    alternate: &'static str,
    directory: ManagedDir,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ComponentExecutionFault {
    None,
    AfterFirstRow,
    CrashAfterFirstRow,
    CrashAfterFirstReplacementQuarantine,
    CrashAfterFirstAncestor,
    CrashBeforeOutcome,
    OutcomePromotionAttempted,
}

struct ObservedRow {
    state: ComponentRecoveryEntryState,
    canonical: Option<ComponentObservedFile>,
    staging: Option<ManagedFileGuard>,
    quarantine: Option<ManagedFileGuard>,
}

pub(crate) async fn execute_component_intent(
    published: ComponentIntentPublished,
) -> ComponentExecutionResult {
    execute_component_intent_inner(published, ComponentExecutionFault::None).await
}

pub(crate) async fn recover_component_transaction(
    lease: ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> ComponentStartupRecoveryResult {
    match run_component_recovery(ComponentRecoveryAuthority::Restart { lease, component }).await {
        RecoveryOwnerResult::NoTransaction(lease) => {
            ComponentStartupRecoveryResult::NoTransaction(lease)
        }
        RecoveryOwnerResult::Transaction(result) => {
            ComponentStartupRecoveryResult::Transaction(result)
        }
        RecoveryOwnerResult::RetryIntent(_) => {
            unreachable!("restart recovery cannot yield an intent retry")
        }
    }
}

pub(crate) async fn recover_component_intent_publication(
    failure: ComponentIntentPublishFailure,
) -> Result<ComponentIntentPublicationRecovery, ComponentIntentPublishFailure> {
    if matches!(
        &failure,
        ComponentIntentPublishFailure::BeforePromotion { .. }
    ) {
        return Err(failure);
    }
    Ok(
        match run_component_recovery(ComponentRecoveryAuthority::IntentPromotionAttempted(
            failure,
        ))
        .await
        {
            RecoveryOwnerResult::RetryIntent(candidate) => {
                ComponentIntentPublicationRecovery::Retry(candidate)
            }
            RecoveryOwnerResult::Transaction(result) => {
                ComponentIntentPublicationRecovery::Transaction(result)
            }
            RecoveryOwnerResult::NoTransaction(_) => {
                unreachable!("attempted publication recovery cannot lose its candidate")
            }
        },
    )
}

pub(crate) async fn retry_component_recovery(
    recovery: ComponentRecoveryRequired,
) -> ComponentRecoveryRetryResult {
    match run_component_recovery(recovery.authority).await {
        RecoveryOwnerResult::NoTransaction(lease) => {
            ComponentRecoveryRetryResult::NoTransaction(lease)
        }
        RecoveryOwnerResult::Transaction(result) => {
            ComponentRecoveryRetryResult::Transaction(result)
        }
        RecoveryOwnerResult::RetryIntent(candidate) => {
            ComponentRecoveryRetryResult::RetryIntent(candidate)
        }
    }
}

#[cfg(test)]
pub(super) async fn execute_component_intent_with_fault(
    published: ComponentIntentPublished,
    fault: ComponentExecutionFault,
) -> ComponentExecutionResult {
    execute_component_intent_inner(published, fault).await
}

async fn execute_component_intent_inner(
    published: ComponentIntentPublished,
    fault: ComponentExecutionFault,
) -> ComponentExecutionResult {
    let shared = Arc::new(Mutex::new(Some(published)));
    let owner_context = Arc::clone(&shared);
    let owner = tokio::spawn(async move {
        let worker_context = Arc::clone(&owner_context);
        let disposition = match run_publication_blocking(move || {
            let slot = worker_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(context) = slot.as_ref() else {
                return BlockingDisposition::RecoveryRequired(None);
            };
            catch_unwind(AssertUnwindSafe(|| {
                execute_component_intent_blocking(context, fault)
            }))
            .unwrap_or(BlockingDisposition::RecoveryRequired(None))
        })
        .await
        {
            Ok(disposition) => disposition,
            Err(_) => BlockingDisposition::RecoveryRequired(None),
        };
        finish_disposition(&owner_context, disposition)
    });
    match owner.await {
        Ok(result) => result,
        Err(_) => finish_disposition(&shared, BlockingDisposition::RecoveryRequired(None)),
    }
}

async fn run_component_recovery(authority: ComponentRecoveryAuthority) -> RecoveryOwnerResult {
    let shared = Arc::new(Mutex::new(Some(authority)));
    let owner_authority = Arc::clone(&shared);
    let owner = tokio::spawn(async move {
        let worker_authority = Arc::clone(&owner_authority);
        let disposition = match run_publication_blocking(move || {
            let mut slot = worker_authority
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let normalized =
                catch_unwind(AssertUnwindSafe(|| normalize_recovery_authority(&mut slot)));
            match normalized {
                Ok(Ok(RecoveryNormalization::Published)) => {}
                Ok(Ok(RecoveryNormalization::NoTransaction)) => {
                    return BlockingDisposition::NoTransaction;
                }
                Ok(Ok(RecoveryNormalization::RetryIntent)) => {
                    return BlockingDisposition::RetryIntent;
                }
                Ok(Err(_)) | Err(_) => return BlockingDisposition::RecoveryRequired(None),
            }
            let Some(ComponentRecoveryAuthority::Published {
                context,
                outcome_guard,
            }) = slot.as_mut()
            else {
                return BlockingDisposition::RecoveryRequired(None);
            };
            let disposition = catch_unwind(AssertUnwindSafe(|| {
                recover_component_transaction_blocking(context, outcome_guard.as_ref())
            }))
            .unwrap_or(BlockingDisposition::RecoveryRequired(None));
            disposition
        })
        .await
        {
            Ok(disposition) => disposition,
            Err(_) => BlockingDisposition::RecoveryRequired(None),
        };
        finish_recovery_disposition(&owner_authority, disposition)
    });
    match owner.await {
        Ok(result) => result,
        Err(_) => finish_recovery_disposition(&shared, BlockingDisposition::RecoveryRequired(None)),
    }
}

fn normalize_recovery_authority(
    slot: &mut Option<ComponentRecoveryAuthority>,
) -> Result<RecoveryNormalization, ComponentTransactionError> {
    let admission = match slot.as_ref().ok_or(ComponentTransactionError)? {
        ComponentRecoveryAuthority::Published { .. } => {
            return Ok(RecoveryNormalization::Published);
        }
        ComponentRecoveryAuthority::Restart { lease, component } => {
            match admit_restart_context(lease, *component, None, true)? {
                Some(admission) => admission,
                None => return Ok(RecoveryNormalization::NoTransaction),
            }
        }
        ComponentRecoveryAuthority::IntentPromotionAttempted(failure) => {
            let ComponentIntentPublishFailure::PromotionAttempted {
                candidate,
                intent_guard,
                ..
            } = failure
            else {
                return Err(ComponentTransactionError);
            };
            let Some(admission) = admit_restart_context(
                &candidate.lease,
                candidate.manifest.component,
                intent_guard.as_ref(),
                false,
            )?
            else {
                let (current_summary, current_authority) = admit_component_preintent(
                    &candidate.lane,
                    &candidate.lease,
                    &candidate.manifest,
                )
                .map_err(tx)?;
                if current_summary != candidate.summary || current_authority != candidate.authority
                {
                    return Err(ComponentTransactionError);
                }
                return Ok(RecoveryNormalization::RetryIntent);
            };
            if admission.outcome_guard.is_some()
                || admission.manifest != candidate.manifest
                || admission.encoded_intent != candidate.encoded_intent
                || !same_lane_identity(&admission.lane, &candidate.lane)?
            {
                return Err(ComponentTransactionError);
            }
            admission
        }
    };

    let authority = slot.take().ok_or(ComponentTransactionError)?;
    let context = match authority {
        ComponentRecoveryAuthority::Restart {
            lease,
            component: _,
        } => ComponentIntentPublished {
            lane: admission.lane,
            lease,
            manifest: admission.manifest,
            encoded_intent: admission.encoded_intent,
            intent_guard: admission.intent_guard,
        },
        ComponentRecoveryAuthority::IntentPromotionAttempted(
            ComponentIntentPublishFailure::PromotionAttempted { candidate, .. },
        ) => {
            let ComponentIntentCandidate {
                lane: _,
                lease,
                manifest: _,
                encoded_intent: _,
                summary,
                authority,
            } = *candidate;
            drop((summary, authority));
            ComponentIntentPublished {
                lane: admission.lane,
                lease,
                manifest: admission.manifest,
                encoded_intent: admission.encoded_intent,
                intent_guard: admission.intent_guard,
            }
        }
        other => {
            *slot = Some(other);
            return Err(ComponentTransactionError);
        }
    };
    *slot = Some(ComponentRecoveryAuthority::Published {
        context,
        outcome_guard: admission.outcome_guard,
    });
    Ok(RecoveryNormalization::Published)
}

fn same_lane_identity(
    left: &ComponentLane,
    right: &ComponentLane,
) -> Result<bool, ComponentTransactionError> {
    Ok(left.component == right.component
        && left.lane.identity().map_err(tx)? == right.lane.identity().map_err(tx)?
        && left.table.identity().map_err(tx)? == right.table.identity().map_err(tx)?
        && left.staging.identity().map_err(tx)? == right.staging.identity().map_err(tx)?
        && left.quarantine.identity().map_err(tx)? == right.quarantine.identity().map_err(tx)?
        && left.ancestors.identity().map_err(tx)? == right.ancestors.identity().map_err(tx)?
        && left.ancestor_records.identity().map_err(tx)?
            == right.ancestor_records.identity().map_err(tx)?
        && left.ancestor_staging.identity().map_err(tx)?
            == right.ancestor_staging.identity().map_err(tx)?)
}

fn admit_restart_context(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    retained_intent_guard: Option<&ManagedFileGuard>,
    clean_marker_free_lane: bool,
) -> Result<Option<ComponentRestartAdmission>, ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    if !publication
        .has_portably_exact_child_name(lane_name)
        .map_err(tx)?
    {
        publication.sync().map_err(tx)?;
        lease.root().sync().map_err(tx)?;
        lease.revalidate().map_err(tx)?;
        if publication
            .has_portably_exact_child_name(lane_name)
            .map_err(tx)?
        {
            return Err(ComponentTransactionError);
        }
        return Ok(None);
    }
    let marker_lane = publication.open_child(lane_name).map_err(tx)?;
    let marker_limit = MAX_COMPONENT_LANE_ENTRIES
        .checked_add(MAX_MANAGED_TEMP_ENTRIES)
        .and_then(|entries| entries.checked_add(1))
        .ok_or(ComponentTransactionError)?;
    let marker_entries = marker_lane.entries_bounded(marker_limit).map_err(tx)?;
    if marker_entries.len() >= marker_limit {
        return Err(ComponentTransactionError);
    }
    let intent_present = marker_entries
        .iter()
        .any(|name| name.as_os_str() == std::ffi::OsStr::new(COMPONENT_INTENT_FILE));
    let outcome_present = marker_entries
        .iter()
        .any(|name| name.as_os_str() == std::ffi::OsStr::new(COMPONENT_OUTCOME_FILE));
    let settlement_present = marker_entries
        .iter()
        .any(|name| name.as_os_str() == std::ffi::OsStr::new(COMPONENT_SETTLEMENT_FILE));
    if settlement_present || (!intent_present && outcome_present) {
        return Err(ComponentTransactionError);
    }
    if !intent_present {
        if retained_intent_guard.is_some() {
            return Err(ComponentTransactionError);
        }
        drop(marker_lane);
        if clean_marker_free_lane {
            admit_empty_marker_free_lane(lease, component)?;
        } else {
            cleanup_recovery_marker_temps(lease, component)?;
        }
        return Ok(None);
    }
    drop(marker_lane);
    cleanup_recovery_marker_temps(lease, component)?;
    let lane = publication.open_child(lane_name).map_err(tx)?;
    let table = lane.open_child(COMPONENT_TABLE_DIRECTORY).map_err(tx)?;
    let staging = lane.open_child(COMPONENT_STAGING_DIRECTORY).map_err(tx)?;
    let quarantine = lane
        .open_child(COMPONENT_QUARANTINE_DIRECTORY)
        .map_err(tx)?;
    let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY).map_err(tx)?;
    let ancestor_records = ancestors
        .open_child(COMPONENT_ANCESTOR_RECORDS_DIRECTORY)
        .map_err(tx)?;
    let ancestor_staging = ancestors
        .open_child(COMPONENT_ANCESTOR_STAGING_DIRECTORY)
        .map_err(tx)?;
    let lane = ComponentLane {
        component,
        lane,
        table,
        staging,
        quarantine,
        ancestors,
        ancestor_records,
        ancestor_staging,
    };
    let names = exact_entry_names(&lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)?;
    let mut expected = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_INTENT_FILE.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    let outcome_present = names.contains(COMPONENT_OUTCOME_FILE);
    if outcome_present {
        expected.insert(COMPONENT_OUTCOME_FILE.to_string());
    }
    if names != expected
        || exact_entry_names(&lane.ancestors, 3).map_err(tx)?
            != BTreeSet::from([
                COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
                COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
            ])
    {
        return Err(ComponentTransactionError);
    }
    let intent_guard = lane
        .lane
        .inspect_regular_file(COMPONENT_INTENT_FILE)
        .map_err(tx)?
        .ok_or(ComponentTransactionError)?;
    if let Some(retained) = retained_intent_guard {
        if retained.identity() != intent_guard.identity()
            || !lane
                .lane
                .file_guard_matches(COMPONENT_INTENT_FILE, retained)
                .map_err(tx)?
        {
            return Err(ComponentTransactionError);
        }
    }
    let encoded_intent = lane
        .lane
        .read_guarded_file_bounded(
            COMPONENT_INTENT_FILE,
            &intent_guard,
            MAX_COMPONENT_INTENT_BYTES as u64,
        )
        .map_err(tx)?;
    let manifest = decode_component_intent_manifest(&encoded_intent).map_err(tx)?;
    if manifest.component != component
        || manifest.root_binding_sha256
            != component_root_binding_sha256(lease.root()).map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    let outcome_guard = outcome_present
        .then(|| {
            lane.lane
                .inspect_regular_file(COMPONENT_OUTCOME_FILE)
                .map_err(tx)?
                .ok_or(ComponentTransactionError)
        })
        .transpose()?;
    lease.revalidate().map_err(tx)?;
    Ok(Some(ComponentRestartAdmission {
        lane,
        manifest,
        encoded_intent,
        intent_guard,
        outcome_guard,
    }))
}

fn admit_empty_marker_free_lane(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<(), ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    let lane = publication.open_child(lane_name).map_err(tx)?;
    let names = exact_entry_names(&lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)?;
    let allowed = BTreeSet::from([
        COMPONENT_TABLE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
    ]);
    if !names.is_subset(&allowed) {
        return Err(ComponentTransactionError);
    }
    for name in [
        COMPONENT_TABLE_DIRECTORY,
        COMPONENT_STAGING_DIRECTORY,
        COMPONENT_QUARANTINE_DIRECTORY,
    ] {
        if names.contains(name) {
            let child = lane.open_child(name).map_err(tx)?;
            if !exact_entry_names(&child, 1).map_err(tx)?.is_empty() {
                return Err(ComponentTransactionError);
            }
        }
    }
    if names.contains(COMPONENT_ANCESTORS_DIRECTORY) {
        let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY).map_err(tx)?;
        let ancestor_names = exact_entry_names(&ancestors, 3).map_err(tx)?;
        let allowed_ancestors = BTreeSet::from([
            COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
            COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
        ]);
        if !ancestor_names.is_subset(&allowed_ancestors) {
            return Err(ComponentTransactionError);
        }
        for name in [
            COMPONENT_ANCESTOR_RECORDS_DIRECTORY,
            COMPONENT_ANCESTOR_STAGING_DIRECTORY,
        ] {
            if ancestor_names.contains(name) {
                let child = ancestors.open_child(name).map_err(tx)?;
                if !exact_entry_names(&child, 1).map_err(tx)?.is_empty() {
                    return Err(ComponentTransactionError);
                }
            }
        }
    }

    lease.revalidate().map_err(tx)?;
    let table = open_or_create_exact_child(&lane, COMPONENT_TABLE_DIRECTORY).map_err(tx)?;
    let staging = open_or_create_exact_child(&lane, COMPONENT_STAGING_DIRECTORY).map_err(tx)?;
    let quarantine =
        open_or_create_exact_child(&lane, COMPONENT_QUARANTINE_DIRECTORY).map_err(tx)?;
    let ancestors = open_or_create_exact_child(&lane, COMPONENT_ANCESTORS_DIRECTORY).map_err(tx)?;
    let records =
        open_or_create_exact_child(&ancestors, COMPONENT_ANCESTOR_RECORDS_DIRECTORY).map_err(tx)?;
    let ancestor_staging =
        open_or_create_exact_child(&ancestors, COMPONENT_ANCESTOR_STAGING_DIRECTORY).map_err(tx)?;
    if !exact_entry_names(&table, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&staging, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&quarantine, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&records, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&ancestor_staging, 1)
            .map_err(tx)?
            .is_empty()
        || exact_entry_names(&ancestors, 3).map_err(tx)?
            != BTreeSet::from([
                COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
                COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
            ])
        || exact_entry_names(&lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)? != allowed
    {
        return Err(ComponentTransactionError);
    }
    table.sync().map_err(tx)?;
    staging.sync().map_err(tx)?;
    quarantine.sync().map_err(tx)?;
    records.sync().map_err(tx)?;
    ancestor_staging.sync().map_err(tx)?;
    ancestors.sync().map_err(tx)?;
    lane.sync().map_err(tx)?;
    publication.sync().map_err(tx)?;
    lease.root().sync().map_err(tx)?;
    lease.revalidate().map_err(tx)
}

fn cleanup_recovery_marker_temps(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<(), ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    if !publication
        .has_portably_exact_child_name(lane_name)
        .map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    let lane = publication.open_child(lane_name).map_err(tx)?;
    let limit = MAX_COMPONENT_LANE_ENTRIES
        .checked_add(MAX_MANAGED_TEMP_ENTRIES)
        .and_then(|entries| entries.checked_add(1))
        .ok_or(ComponentTransactionError)?;
    let entries = lane.entries_bounded(limit).map_err(tx)?;
    if entries.len() >= limit {
        return Err(ComponentTransactionError);
    }
    let mut temporary = Vec::new();
    temporary
        .try_reserve_exact(entries.len().min(MAX_MANAGED_TEMP_ENTRIES))
        .map_err(tx)?;
    let mut known = BTreeSet::new();
    for name in entries {
        let name = name.into_string().map_err(tx)?;
        if matches!(
            name.as_str(),
            COMPONENT_TABLE_DIRECTORY
                | COMPONENT_STAGING_DIRECTORY
                | COMPONENT_QUARANTINE_DIRECTORY
                | COMPONENT_ANCESTORS_DIRECTORY
                | COMPONENT_INTENT_FILE
                | COMPONENT_OUTCOME_FILE
                | COMPONENT_SETTLEMENT_FILE
        ) {
            known.insert(name);
            continue;
        }
        if !validate_managed_temp_name(&name).map_err(tx)?
            || temporary.len() >= MAX_MANAGED_TEMP_ENTRIES
        {
            return Err(ComponentTransactionError);
        }
        let guard = lane
            .inspect_regular_file(&name)
            .map_err(tx)?
            .ok_or(ComponentTransactionError)?;
        if guard.size() > MAX_COMPONENT_INTENT_BYTES as u64
            || !lane.managed_temp_is_orphan(&name, &guard).map_err(tx)?
        {
            return Err(ComponentTransactionError);
        }
        temporary.push(ComponentPlannedFile {
            name,
            size: guard.size(),
            identity: guard.identity(),
        });
    }
    let mut expected_known = BTreeSet::from([
        COMPONENT_TABLE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
    ]);
    if known.contains(COMPONENT_INTENT_FILE) {
        expected_known.insert(COMPONENT_INTENT_FILE.to_string());
    }
    if known.contains(COMPONENT_OUTCOME_FILE) {
        expected_known.insert(COMPONENT_OUTCOME_FILE.to_string());
    }
    if known != expected_known {
        return Err(ComponentTransactionError);
    }
    let _table = lane.open_child(COMPONENT_TABLE_DIRECTORY).map_err(tx)?;
    let _staging = lane.open_child(COMPONENT_STAGING_DIRECTORY).map_err(tx)?;
    let _quarantine = lane
        .open_child(COMPONENT_QUARANTINE_DIRECTORY)
        .map_err(tx)?;
    let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY).map_err(tx)?;
    let records = ancestors
        .open_child(COMPONENT_ANCESTOR_RECORDS_DIRECTORY)
        .map_err(tx)?;
    let ancestor_staging = ancestors
        .open_child(COMPONENT_ANCESTOR_STAGING_DIRECTORY)
        .map_err(tx)?;
    if exact_entry_names(&ancestors, 3).map_err(tx)?
        != BTreeSet::from([
            COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
            COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
        ])
    {
        return Err(ComponentTransactionError);
    }
    ancestor_staging.revalidate().map_err(tx)?;
    let records_plan = plan_directory_files(
        &records,
        MAX_COMPONENT_ANCESTOR_SHARDS,
        MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES as u64,
        |name| {
            name.strip_suffix(COMPONENT_ANCESTOR_RECORD_FILE_SUFFIX)
                .and_then(|index| parse_fixed_decimal(index, 6))
                .and_then(|index| component_ancestor_bucket_name(index).ok().map(|_| index))
                .is_some()
        },
    )
    .map_err(tx)?;
    for planned in &temporary {
        let guard = inspect_planned_file(&lane, planned).map_err(tx)?;
        if !lane
            .managed_temp_is_orphan(&planned.name, &guard)
            .map_err(tx)?
        {
            return Err(ComponentTransactionError);
        }
    }
    validate_planned_file_entries(
        &records,
        &records_plan.owned,
        &records_plan.temporary,
        MAX_COMPONENT_ANCESTOR_SHARDS,
        true,
    )
    .map_err(tx)?;
    remove_planned_temps(&lane, &temporary).map_err(tx)?;
    remove_planned_temps(&records, &records_plan.temporary).map_err(tx)?;
    records.sync().map_err(tx)?;
    ancestors.sync().map_err(tx)?;
    lane.sync().map_err(tx)?;
    publication.sync().map_err(tx)?;
    lease.root().sync().map_err(tx)?;
    lease.revalidate().map_err(tx)
}

fn execute_component_intent_blocking(
    published: &ComponentIntentPublished,
    fault: ComponentExecutionFault,
) -> BlockingDisposition {
    let summary = match validate_published_and_replay(published, false) {
        Ok(summary) => summary,
        Err(_) => return BlockingDisposition::RecoveryRequired(None),
    };
    let ComponentTableSummary {
        created_ancestors, ..
    } = summary;
    let ancestor_authority =
        match ComponentAncestorJournalAuthority::new(&published.encoded_intent, &created_ancestors)
        {
            Ok(authority) => authority,
            Err(_) => return BlockingDisposition::RecoveryRequired(None),
        };

    let execution =
        create_and_promote_ancestors(published, &ancestor_authority, &created_ancestors, fault)
            .and_then(|()| {
                observe_all_rows(published, ComponentRecoveryDecision::Rollback, true)?;
                execute_rows_forward(published, fault)?;
                postcheck(
                    published,
                    &ancestor_authority,
                    &created_ancestors,
                    ComponentRecoveryDecision::Commit,
                )
            });

    if matches!(
        fault,
        ComponentExecutionFault::CrashAfterFirstRow
            | ComponentExecutionFault::CrashAfterFirstReplacementQuarantine
            | ComponentExecutionFault::CrashAfterFirstAncestor
            | ComponentExecutionFault::CrashBeforeOutcome
    ) {
        return BlockingDisposition::RecoveryRequired(None);
    }

    if execution.is_ok() {
        match publish_outcome(
            published,
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None,
            fault,
        ) {
            Ok(outcome_guard) => return BlockingDisposition::Committed(outcome_guard),
            Err(OutcomePublicationFailure::PromotionAttempted(outcome_guard)) => {
                return BlockingDisposition::RecoveryRequired(outcome_guard);
            }
            Err(OutcomePublicationFailure::BeforePromotion) => {}
        }
    }

    if rollback_live(published, &ancestor_authority, &created_ancestors).is_err() {
        return BlockingDisposition::RecoveryRequired(None);
    }
    match publish_outcome(
        published,
        ComponentTerminalOutcome::RolledBack,
        ComponentRollbackEffect::Execution,
        ComponentExecutionFault::None,
    ) {
        Ok(outcome_guard) => BlockingDisposition::RolledBack(outcome_guard),
        Err(OutcomePublicationFailure::PromotionAttempted(outcome_guard)) => {
            BlockingDisposition::RecoveryRequired(outcome_guard)
        }
        Err(OutcomePublicationFailure::BeforePromotion) => {
            BlockingDisposition::RecoveryRequired(None)
        }
    }
}

fn recover_component_transaction_blocking(
    published: &ComponentIntentPublished,
    retained_outcome_guard: Option<&ManagedFileGuard>,
) -> BlockingDisposition {
    let outcome = match read_recovery_outcome(published, retained_outcome_guard) {
        Ok(outcome) => outcome,
        Err(_) => return BlockingDisposition::RecoveryRequired(None),
    };
    let summary = match validate_published_and_replay(published, outcome.is_some()) {
        Ok(summary) => summary,
        Err(_) => return BlockingDisposition::RecoveryRequired(outcome.map(|(_, guard)| guard)),
    };
    let ComponentTableSummary {
        created_ancestors, ..
    } = summary;
    let ancestor_authority =
        match ComponentAncestorJournalAuthority::new(&published.encoded_intent, &created_ancestors)
        {
            Ok(authority) => authority,
            Err(_) => {
                return BlockingDisposition::RecoveryRequired(outcome.map(|(_, guard)| guard));
            }
        };
    let row_plan = match plan_all_rows(published) {
        Ok(plan) => plan,
        Err(_) => return BlockingDisposition::RecoveryRequired(outcome.map(|(_, guard)| guard)),
    };
    let ancestor_plan = match admit_ancestor_recovery(
        published,
        &ancestor_authority,
        &created_ancestors,
        outcome.is_none(),
    ) {
        Ok(plan) => plan,
        Err(_) => {
            return BlockingDisposition::RecoveryRequired(outcome.map(|(_, guard)| guard));
        }
    };
    let all_ancestors_committed = ancestor_plan.durable_shards == ancestor_authority.shard_count()
        && ancestor_plan.canonical_records == ancestor_authority.total_records();
    if !all_ancestors_committed && !row_plan.all_pristine {
        return BlockingDisposition::RecoveryRequired(outcome.map(|(_, guard)| guard));
    }

    if let Some((record, guard)) = outcome {
        let terminal_is_exact = match record.terminal {
            ComponentTerminalOutcome::Committed => {
                record.effect == ComponentRollbackEffect::None
                    && all_ancestors_committed
                    && row_plan.decision == ComponentRecoveryDecision::Commit
                    && postcheck_with_outcome(
                        published,
                        &ancestor_authority,
                        &created_ancestors,
                        ComponentRecoveryDecision::Commit,
                        true,
                    )
                    .is_ok()
            }
            ComponentTerminalOutcome::RolledBack => {
                matches!(
                    record.effect,
                    ComponentRollbackEffect::Execution | ComponentRollbackEffect::Reconciliation
                ) && row_plan.all_pristine
                    && ancestor_plan.canonical_records == 0
                    && postcheck_with_outcome(
                        published,
                        &ancestor_authority,
                        &created_ancestors,
                        ComponentRecoveryDecision::Rollback,
                        true,
                    )
                    .is_ok()
            }
        };
        if !terminal_is_exact {
            return BlockingDisposition::RecoveryRequired(Some(guard));
        }
        return match record.terminal {
            ComponentTerminalOutcome::Committed => BlockingDisposition::Committed(guard),
            ComponentTerminalOutcome::RolledBack => BlockingDisposition::RolledBack(guard),
        };
    }

    if all_ancestors_committed && row_plan.decision == ComponentRecoveryDecision::Commit {
        if postcheck(
            published,
            &ancestor_authority,
            &created_ancestors,
            ComponentRecoveryDecision::Commit,
        )
        .is_err()
        {
            return BlockingDisposition::RecoveryRequired(None);
        }
        return match publish_outcome(
            published,
            ComponentTerminalOutcome::Committed,
            ComponentRollbackEffect::None,
            ComponentExecutionFault::None,
        ) {
            Ok(guard) => BlockingDisposition::Committed(guard),
            Err(OutcomePublicationFailure::PromotionAttempted(guard)) => {
                BlockingDisposition::RecoveryRequired(guard)
            }
            Err(OutcomePublicationFailure::BeforePromotion) => {
                BlockingDisposition::RecoveryRequired(None)
            }
        };
    }

    let rows_rolled_back = rollback_rows(published).is_ok()
        && matches!(plan_all_rows(published), Ok(plan) if plan.all_pristine);
    if !row_plan.rollback_reachable
        || !rows_rolled_back
        || rollback_ancestors(published, &ancestor_authority, &created_ancestors).is_err()
        || postcheck(
            published,
            &ancestor_authority,
            &created_ancestors,
            ComponentRecoveryDecision::Rollback,
        )
        .is_err()
    {
        return BlockingDisposition::RecoveryRequired(None);
    }
    match publish_outcome(
        published,
        ComponentTerminalOutcome::RolledBack,
        ComponentRollbackEffect::Reconciliation,
        ComponentExecutionFault::None,
    ) {
        Ok(guard) => BlockingDisposition::RolledBack(guard),
        Err(OutcomePublicationFailure::PromotionAttempted(guard)) => {
            BlockingDisposition::RecoveryRequired(guard)
        }
        Err(OutcomePublicationFailure::BeforePromotion) => {
            BlockingDisposition::RecoveryRequired(None)
        }
    }
}

fn read_recovery_outcome(
    published: &ComponentIntentPublished,
    retained: Option<&ManagedFileGuard>,
) -> Result<Option<(ComponentOutcomeRecord, ManagedFileGuard)>, ComponentTransactionError> {
    let Some(guard) = published
        .lane
        .lane
        .inspect_regular_file(COMPONENT_OUTCOME_FILE)
        .map_err(tx)?
    else {
        if retained.is_some() {
            return Err(ComponentTransactionError);
        }
        return Ok(None);
    };
    if let Some(retained) = retained {
        if retained.identity() != guard.identity()
            || !published
                .lane
                .lane
                .file_guard_matches(COMPONENT_OUTCOME_FILE, retained)
                .map_err(tx)?
        {
            return Err(ComponentTransactionError);
        }
    }
    if guard.size() != COMPONENT_OUTCOME_BYTES as u64 {
        return Err(ComponentTransactionError);
    }
    let bytes = published
        .lane
        .lane
        .read_guarded_file_bounded(
            COMPONENT_OUTCOME_FILE,
            &guard,
            COMPONENT_OUTCOME_BYTES as u64,
        )
        .map_err(tx)?;
    let outcome = decode_component_outcome(&bytes).map_err(tx)?;
    outcome
        .binds_intent(&published.manifest, &published.encoded_intent)
        .map_err(tx)?;
    Ok(Some((outcome, guard)))
}

fn validate_published_and_replay(
    published: &ComponentIntentPublished,
    outcome_present: bool,
) -> Result<ComponentTableSummary, ComponentTransactionError> {
    published.lease.revalidate().map_err(tx)?;
    if published.manifest.component != published.lane.component
        || published.intent_guard.size()
            != u64::try_from(published.encoded_intent.len()).map_err(tx)?
        || published
            .lane
            .lane
            .read_guarded_file_bounded(
                COMPONENT_INTENT_FILE,
                &published.intent_guard,
                MAX_COMPONENT_INTENT_BYTES as u64,
            )
            .map_err(tx)?
            != published.encoded_intent
    {
        return Err(ComponentTransactionError);
    }
    if component_root_binding_sha256(published.lease.root()).map_err(tx)?
        != published.manifest.root_binding_sha256
    {
        return Err(ComponentTransactionError);
    }
    validate_terminal_topology(published, outcome_present)?;
    let mut parser = ComponentTableParser::new(published.manifest.clone()).map_err(tx)?;
    for shard_index in 0..published.manifest.shards.len() {
        let bytes = read_table_shard_bytes(published, shard_index)?;
        parser.parse_next(&bytes).map_err(tx)?;
    }
    let summary = parser.finish().map_err(tx)?;
    sync_transaction_roots(published)?;
    Ok(summary)
}

fn create_and_promote_ancestors(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
    fault: ComponentExecutionFault,
) -> Result<(), ComponentTransactionError> {
    for shard_index in 0..authority.shard_count() {
        let first = shard_index
            .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
            .ok_or(ComponentTransactionError)?;
        let count = (targets.len() - first).min(COMPONENT_ANCESTOR_RECORDS_PER_SHARD);
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = published
            .lane
            .ancestor_staging
            .create_child_new(&bucket_name)
            .map_err(tx)?;
        let mut slots = Vec::new();
        let mut records = Vec::new();
        slots.try_reserve_exact(count).map_err(tx)?;
        records.try_reserve_exact(count).map_err(tx)?;
        let staging_result = (|| {
            for row_in_shard in 0..count {
                let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
                slots.push(bucket.create_child_new(&slot_name).map_err(tx)?);
                let ordinal = first + row_in_shard;
                let identity = slots
                    .last()
                    .ok_or(ComponentTransactionError)?
                    .identity()
                    .map_err(tx)?;
                records.push(
                    ComponentAncestorJournalRecord::new(
                        ordinal,
                        targets[ordinal].clone(),
                        identity,
                    )
                    .map_err(tx)?,
                );
            }
            Ok::<_, ComponentTransactionError>(())
        })();
        if staging_result.is_err() {
            cleanup_unjournaled_bucket(
                &published.lane.ancestor_staging,
                &bucket_name,
                bucket,
                slots,
            )?;
            return Err(ComponentTransactionError);
        }
        bucket.sync().map_err(tx)?;
        published.lane.ancestor_staging.sync().map_err(tx)?;
        let journal = authority.create_shard(shard_index, records).map_err(tx)?;
        let encoded = authority.encode_shard(&journal).map_err(tx)?;
        let record_name = component_ancestor_record_file_name(shard_index).map_err(tx)?;
        let record_guard = match published
            .lane
            .ancestor_records
            .write_new_exact_retained(&record_name, &encoded)
        {
            Ok(guard) => guard,
            Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_)) => {
                cleanup_unjournaled_bucket(
                    &published.lane.ancestor_staging,
                    &bucket_name,
                    bucket,
                    slots,
                )?;
                return Err(ComponentTransactionError);
            }
            Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { .. }) => {
                return Err(ComponentTransactionError);
            }
        };
        if published
            .lane
            .ancestor_records
            .read_guarded_file_bounded(
                &record_name,
                &record_guard,
                MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES as u64,
            )
            .map_err(tx)?
            != encoded
        {
            return Err(ComponentTransactionError);
        }
        published.lane.ancestor_records.sync().map_err(tx)?;
        sync_transaction_roots(published)?;

        for (row_in_shard, slot) in slots.into_iter().enumerate() {
            let ordinal = first + row_in_shard;
            let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
            let (destination, destination_name) =
                canonical_ancestor_parent(published, &targets[ordinal])?;
            let moved = bucket
                .move_child_guarded_no_replace(&slot_name, slot, &destination, &destination_name)
                .map_err(directory_move_error)?;
            bucket.sync().map_err(tx)?;
            destination.sync().map_err(tx)?;
            moved.sync().map_err(tx)?;
            drop(moved);
            sync_transaction_roots(published)?;
            if fault == ComponentExecutionFault::CrashAfterFirstAncestor
                && shard_index == 0
                && row_in_shard == 0
            {
                return Err(ComponentTransactionError);
            }
        }
    }
    prove_ancestors_committed(published, authority, targets)
}

fn cleanup_unjournaled_bucket(
    parent: &ManagedDir,
    bucket_name: &str,
    bucket: ManagedDir,
    slots: Vec<ManagedDir>,
) -> Result<(), ComponentTransactionError> {
    for (index, slot) in slots.into_iter().enumerate().rev() {
        let name = component_slot_name(index).map_err(tx)?;
        if bucket
            .remove_empty_child_guarded(&name, ANCESTOR_SLOT_PARK_A, slot)
            .map_err(tx)?
            != ManagedEmptyChildRemoval::Removed
        {
            return Err(ComponentTransactionError);
        }
        bucket.sync().map_err(tx)?;
    }
    if parent
        .remove_empty_child_guarded(bucket_name, COMPONENT_BUCKET_PARK_A, bucket)
        .map_err(tx)?
        != ManagedEmptyChildRemoval::Removed
    {
        return Err(ComponentTransactionError);
    }
    parent.sync().map_err(tx)?;
    Ok(())
}

fn execute_rows_forward(
    published: &ComponentIntentPublished,
    fault: ComponentExecutionFault,
) -> Result<(), ComponentTransactionError> {
    let mut parser = ComponentTableParser::new(published.manifest.clone()).map_err(tx)?;
    for shard_index in 0..published.manifest.shards.len() {
        let bytes = read_table_shard_bytes(published, shard_index)?;
        let shard = parser.parse_next(&bytes).map_err(tx)?;
        let bucket_name = component_bucket_name(shard_index).map_err(tx)?;
        let staging = published
            .lane
            .staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        let quarantine = published
            .lane
            .quarantine
            .open_child(&bucket_name)
            .map_err(tx)?;
        for (row_in_shard, row) in shard.rows.iter().enumerate() {
            let observed = observe_row(published, row, &staging, &quarantine, row_in_shard)?;
            match observed.state {
                ComponentRecoveryEntryState::Exact => {}
                ComponentRecoveryEntryState::StagedNew => move_staged_to_canonical(
                    published,
                    row,
                    &staging,
                    row_in_shard,
                    observed.staging.ok_or(ComponentTransactionError)?,
                )?,
                ComponentRecoveryEntryState::StagedReplacement => {
                    let canonical = observed.canonical.ok_or(ComponentTransactionError)?;
                    let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
                    canonical
                        .parent
                        .rename_guarded_file_no_replace(
                            &canonical.file_name,
                            &canonical.guard,
                            &quarantine,
                            &slot_name,
                        )
                        .map_err(tx)?;
                    sync_file_move(
                        published,
                        &canonical.parent,
                        &canonical.file_name,
                        &quarantine,
                        &slot_name,
                        &canonical.guard,
                        canonical.size,
                        canonical.sha1,
                    )?;
                    let intermediate =
                        observe_row(published, row, &staging, &quarantine, row_in_shard)?;
                    if intermediate.state != ComponentRecoveryEntryState::QuarantinedReplacement {
                        return Err(ComponentTransactionError);
                    }
                    if fault == ComponentExecutionFault::CrashAfterFirstReplacementQuarantine
                        && shard_index == 0
                        && row_in_shard == 0
                    {
                        return Err(ComponentTransactionError);
                    }
                    move_staged_to_canonical(
                        published,
                        row,
                        &staging,
                        row_in_shard,
                        intermediate.staging.ok_or(ComponentTransactionError)?,
                    )?;
                }
                _ => return Err(ComponentTransactionError),
            }
            if matches!(
                fault,
                ComponentExecutionFault::AfterFirstRow
                    | ComponentExecutionFault::CrashAfterFirstRow
            ) && shard_index == 0
                && row_in_shard == 0
            {
                return Err(ComponentTransactionError);
            }
        }
    }
    parser.finish().map_err(tx)?;
    Ok(())
}

fn move_staged_to_canonical(
    published: &ComponentIntentPublished,
    row: &ComponentTableRow,
    staging: &ManagedDir,
    row_in_shard: usize,
    guard: ManagedFileGuard,
) -> Result<(), ComponentTransactionError> {
    let plan =
        plan_component_canonical_path(published.lease.root(), published.lane.component, &row.path)
            .map_err(tx)?;
    let parent = plan.parent().ok_or(ComponentTransactionError)?;
    let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
    staging
        .rename_guarded_file_no_replace(&slot_name, &guard, parent, plan.file_name())
        .map_err(tx)?;
    sync_file_move(
        published,
        staging,
        &slot_name,
        parent,
        plan.file_name(),
        &guard,
        row.final_size,
        row.final_sha1,
    )
}

fn rollback_live(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
) -> Result<(), ComponentTransactionError> {
    observe_all_rows(published, ComponentRecoveryDecision::Rollback, false)?;
    rollback_rows(published)?;
    observe_all_rows(published, ComponentRecoveryDecision::Rollback, true)?;
    rollback_ancestors(published, authority, targets)?;
    postcheck(
        published,
        authority,
        targets,
        ComponentRecoveryDecision::Rollback,
    )
}

fn rollback_rows(published: &ComponentIntentPublished) -> Result<(), ComponentTransactionError> {
    for shard_index in (0..published.manifest.shards.len()).rev() {
        let shard = read_authenticated_table_shard(published, shard_index)?;
        let bucket_name = component_bucket_name(shard_index).map_err(tx)?;
        let staging = published
            .lane
            .staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        let quarantine = published
            .lane
            .quarantine
            .open_child(&bucket_name)
            .map_err(tx)?;
        for (row_in_shard, row) in shard.rows.iter().enumerate().rev() {
            let observed = observe_row(published, row, &staging, &quarantine, row_in_shard)?;
            match observed.state {
                ComponentRecoveryEntryState::Exact
                | ComponentRecoveryEntryState::StagedNew
                | ComponentRecoveryEntryState::StagedReplacement => {}
                ComponentRecoveryEntryState::CommittedNew => move_canonical_to_staging(
                    published,
                    &staging,
                    row_in_shard,
                    observed.canonical.ok_or(ComponentTransactionError)?,
                )?,
                ComponentRecoveryEntryState::QuarantinedReplacement => {
                    move_quarantine_to_canonical(
                        published,
                        row,
                        &quarantine,
                        row_in_shard,
                        observed.quarantine.ok_or(ComponentTransactionError)?,
                    )?;
                }
                ComponentRecoveryEntryState::CommittedReplacement => {
                    move_canonical_to_staging(
                        published,
                        &staging,
                        row_in_shard,
                        observed.canonical.ok_or(ComponentTransactionError)?,
                    )?;
                    let intermediate =
                        observe_row(published, row, &staging, &quarantine, row_in_shard)?;
                    if intermediate.state != ComponentRecoveryEntryState::QuarantinedReplacement {
                        return Err(ComponentTransactionError);
                    }
                    move_quarantine_to_canonical(
                        published,
                        row,
                        &quarantine,
                        row_in_shard,
                        intermediate.quarantine.ok_or(ComponentTransactionError)?,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn move_canonical_to_staging(
    published: &ComponentIntentPublished,
    staging: &ManagedDir,
    row_in_shard: usize,
    canonical: ComponentObservedFile,
) -> Result<(), ComponentTransactionError> {
    let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
    canonical
        .parent
        .rename_guarded_file_no_replace(&canonical.file_name, &canonical.guard, staging, &slot_name)
        .map_err(tx)?;
    sync_file_move(
        published,
        &canonical.parent,
        &canonical.file_name,
        staging,
        &slot_name,
        &canonical.guard,
        canonical.size,
        canonical.sha1,
    )
}

fn move_quarantine_to_canonical(
    published: &ComponentIntentPublished,
    row: &ComponentTableRow,
    quarantine: &ManagedDir,
    row_in_shard: usize,
    guard: ManagedFileGuard,
) -> Result<(), ComponentTransactionError> {
    let plan =
        plan_component_canonical_path(published.lease.root(), published.lane.component, &row.path)
            .map_err(tx)?;
    let parent = plan.parent().ok_or(ComponentTransactionError)?;
    let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
    let prior = row.prior.as_ref().ok_or(ComponentTransactionError)?;
    quarantine
        .rename_guarded_file_no_replace(&slot_name, &guard, parent, plan.file_name())
        .map_err(tx)?;
    sync_file_move(
        published,
        quarantine,
        &slot_name,
        parent,
        plan.file_name(),
        &guard,
        prior.size,
        prior.sha1,
    )
}

fn observe_all_rows(
    published: &ComponentIntentPublished,
    expected: ComponentRecoveryDecision,
    require_pristine_rollback: bool,
) -> Result<(), ComponentTransactionError> {
    let plan = plan_all_rows(published)?;
    if (require_pristine_rollback && !plan.all_pristine)
        || match expected {
            ComponentRecoveryDecision::Commit => plan.decision != ComponentRecoveryDecision::Commit,
            ComponentRecoveryDecision::Rollback => !plan.rollback_reachable,
        }
    {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn plan_all_rows(
    published: &ComponentIntentPublished,
) -> Result<ComponentRecoveryPlan, ComponentTransactionError> {
    let expected_rows = usize::try_from(published.manifest.total_rows).map_err(tx)?;
    let mut planner = ComponentRecoveryPlanner::new(expected_rows).map_err(tx)?;
    let mut parser = ComponentTableParser::new(published.manifest.clone()).map_err(tx)?;
    for shard_index in 0..published.manifest.shards.len() {
        let bytes = read_table_shard_bytes(published, shard_index)?;
        let shard = parser.parse_next(&bytes).map_err(tx)?;
        let bucket_name = component_bucket_name(shard_index).map_err(tx)?;
        let staging = published
            .lane
            .staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        let quarantine = published
            .lane
            .quarantine
            .open_child(&bucket_name)
            .map_err(tx)?;
        let mut staging_names = BTreeSet::new();
        let mut quarantine_names = BTreeSet::new();
        for (row_in_shard, row) in shard.rows.iter().enumerate() {
            let observed = observe_row(published, row, &staging, &quarantine, row_in_shard)?;
            let observation = recovery_observation(row, &observed)?;
            planner.observe(row, observation).map_err(tx)?;
            let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
            if observed.staging.is_some() {
                staging_names.insert(slot_name.clone());
            }
            if observed.quarantine.is_some() {
                quarantine_names.insert(slot_name);
            }
        }
        if exact_entry_names(&staging, shard.rows.len() + 1).map_err(tx)? != staging_names
            || exact_entry_names(&quarantine, shard.rows.len() + 1).map_err(tx)? != quarantine_names
        {
            return Err(ComponentTransactionError);
        }
    }
    parser.finish().map_err(tx)?;
    let plan = planner.finish().map_err(tx)?;
    sync_transaction_roots(published)?;
    Ok(plan)
}

fn observe_row(
    published: &ComponentIntentPublished,
    row: &ComponentTableRow,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    row_in_shard: usize,
) -> Result<ObservedRow, ComponentTransactionError> {
    let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
    let staging_guard = exact_file(staging, &slot_name, row.final_size, row.final_sha1)?;
    let quarantine_guard = match &row.prior {
        Some(prior) if !row.prior_is_final() => {
            exact_file(quarantine, &slot_name, prior.size, prior.sha1)?
        }
        _ => {
            if quarantine
                .inspect_regular_file(&slot_name)
                .map_err(tx)?
                .is_some()
            {
                return Err(ComponentTransactionError);
            }
            None
        }
    };
    let canonical_plan =
        plan_component_canonical_path(published.lease.root(), published.lane.component, &row.path)
            .map_err(tx)?;
    let canonical = match canonical_plan.observe().map_err(tx)? {
        ComponentCanonicalObservation::Absent => None,
        ComponentCanonicalObservation::Regular(file) => Some(file),
    };
    let observation = recovery_observation_from_parts(
        row,
        canonical.as_ref(),
        staging_guard.is_some(),
        quarantine_guard.is_some(),
    );
    let mut planner = ComponentRecoveryPlanner::new(1).map_err(tx)?;
    let state = planner.observe(row, observation).map_err(tx)?;
    Ok(ObservedRow {
        state,
        canonical,
        staging: staging_guard,
        quarantine: quarantine_guard,
    })
}

fn recovery_observation(
    row: &ComponentTableRow,
    observed: &ObservedRow,
) -> Result<ComponentRecoveryObservation, ComponentTransactionError> {
    Ok(recovery_observation_from_parts(
        row,
        observed.canonical.as_ref(),
        observed.staging.is_some(),
        observed.quarantine.is_some(),
    ))
}

fn recovery_observation_from_parts(
    row: &ComponentTableRow,
    canonical: Option<&ComponentObservedFile>,
    stage_present: bool,
    quarantine_present: bool,
) -> ComponentRecoveryObservation {
    let canonical = match canonical {
        None => ComponentObservedCanonical::Absent,
        Some(file) if file.size == row.final_size && file.sha1 == row.final_sha1 => {
            ComponentObservedCanonical::Source
        }
        Some(file)
            if row
                .prior
                .as_ref()
                .is_some_and(|prior| file.size == prior.size && file.sha1 == prior.sha1) =>
        {
            ComponentObservedCanonical::Prior
        }
        Some(_) => ComponentObservedCanonical::Other,
    };
    ComponentRecoveryObservation {
        canonical,
        stage_present,
        quarantine_present,
    }
}

fn exact_file(
    directory: &ManagedDir,
    name: &str,
    size: u64,
    sha1: [u8; 20],
) -> Result<Option<ManagedFileGuard>, ComponentTransactionError> {
    let Some(guard) = directory.inspect_regular_file(name).map_err(tx)? else {
        return Ok(None);
    };
    if guard.size() != size
        || directory
            .sha1_guarded_file_bytes(name, &guard, MAX_TIER2_ARTIFACT_BYTES)
            .map_err(tx)?
            != sha1
    {
        return Err(ComponentTransactionError);
    }
    Ok(Some(guard))
}

fn rollback_ancestors(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
) -> Result<(), ComponentTransactionError> {
    let durable_prefix = durable_ancestor_prefix(published, authority)?;
    for shard_index in (0..durable_prefix).rev() {
        let shard = read_ancestor_journal(published, authority, shard_index)?;
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = published
            .lane
            .ancestor_staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        for record in shard.records().iter().rev() {
            let row_in_shard = record.ordinal() % COMPONENT_ANCESTOR_RECORDS_PER_SHARD;
            let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
            if bucket
                .has_portably_exact_child_name(&slot_name)
                .map_err(tx)?
            {
                let slot = bucket.open_child(&slot_name).map_err(tx)?;
                if !record.matches_identity(slot.identity().map_err(tx)?)
                    || !slot.entries_bounded(1).map_err(tx)?.is_empty()
                {
                    return Err(ComponentTransactionError);
                }
                continue;
            }
            let (parent, name) = canonical_ancestor_parent(published, record.target())?;
            let canonical = parent.open_child(&name).map_err(tx)?;
            if !record.matches_identity(canonical.identity().map_err(tx)?)
                || !canonical.entries_bounded(1).map_err(tx)?.is_empty()
            {
                return Err(ComponentTransactionError);
            }
            let moved = parent
                .move_child_guarded_no_replace(&name, canonical, &bucket, &slot_name)
                .map_err(directory_move_error)?;
            parent.sync().map_err(tx)?;
            bucket.sync().map_err(tx)?;
            moved.sync().map_err(tx)?;
            drop(moved);
            sync_transaction_roots(published)?;
        }
    }
    prove_ancestors_rolled_back(published, authority, targets)
}

fn prove_ancestors_committed(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
) -> Result<(), ComponentTransactionError> {
    if durable_ancestor_prefix(published, authority)? != authority.shard_count() {
        return Err(ComponentTransactionError);
    }
    for shard_index in 0..authority.shard_count() {
        let shard = read_ancestor_journal(published, authority, shard_index)?;
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = published
            .lane
            .ancestor_staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        if !exact_entry_names(&bucket, COMPONENT_ANCESTOR_RECORDS_PER_SHARD + 1)
            .map_err(tx)?
            .is_empty()
        {
            return Err(ComponentTransactionError);
        }
        for record in shard.records() {
            let (parent, name) = canonical_ancestor_parent(published, record.target())?;
            let canonical = parent.open_child(&name).map_err(tx)?;
            if !record.matches_identity(canonical.identity().map_err(tx)?) {
                return Err(ComponentTransactionError);
            }
        }
    }
    if authority.total_records() != targets.len() {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn prove_ancestors_rolled_back(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
) -> Result<(), ComponentTransactionError> {
    let durable_prefix = durable_ancestor_prefix(published, authority)?;
    for shard_index in 0..durable_prefix {
        let shard = read_ancestor_journal(published, authority, shard_index)?;
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = published
            .lane
            .ancestor_staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        let expected = shard
            .records()
            .iter()
            .map(|record| {
                component_slot_name(record.ordinal() % COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
            })
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(tx)?;
        if exact_entry_names(&bucket, COMPONENT_ANCESTOR_RECORDS_PER_SHARD + 1).map_err(tx)?
            != expected
        {
            return Err(ComponentTransactionError);
        }
        for record in shard.records() {
            let slot_name =
                component_slot_name(record.ordinal() % COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
                    .map_err(tx)?;
            let slot = bucket.open_child(&slot_name).map_err(tx)?;
            if !record.matches_identity(slot.identity().map_err(tx)?)
                || !slot.entries_bounded(1).map_err(tx)?.is_empty()
            {
                return Err(ComponentTransactionError);
            }
        }
    }
    if authority.total_records() != targets.len() {
        return Err(ComponentTransactionError);
    }
    for target in targets {
        if canonical_ancestor_is_present(published, target)? {
            return Err(ComponentTransactionError);
        }
    }
    Ok(())
}

fn canonical_ancestor_is_present(
    published: &ComponentIntentPublished,
    target: &ComponentCreatedAncestor,
) -> Result<bool, ComponentTransactionError> {
    published.lease.revalidate().map_err(tx)?;
    let root = published.lease.root();
    let component_root = component_lane_name(published.lane.component);
    let present = (|| match target {
        ComponentCreatedAncestor::ComponentRoot => root
            .has_portably_exact_child_name(component_root)
            .map_err(tx),
        ComponentCreatedAncestor::Relative(path) => {
            if !root
                .has_portably_exact_child_name(component_root)
                .map_err(tx)?
            {
                return Ok(false);
            }
            let mut parent = root.open_child(component_root).map_err(tx)?;
            let segments = path.as_str().split('/').collect::<Vec<_>>();
            for (index, segment) in segments.iter().enumerate() {
                if !parent.has_portably_exact_child_name(segment).map_err(tx)? {
                    return Ok(false);
                }
                if index + 1 == segments.len() {
                    return Ok(true);
                }
                parent = parent.open_child(segment).map_err(tx)?;
            }
            Err(ComponentTransactionError)
        }
    })()?;
    published.lease.revalidate().map_err(tx)?;
    Ok(present)
}

fn durable_ancestor_prefix(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
) -> Result<usize, ComponentTransactionError> {
    let bucket_names = exact_entry_names(
        &published.lane.ancestor_staging,
        MAX_COMPONENT_ANCESTOR_SHARDS + 1,
    )
    .map_err(tx)?;
    let record_names = exact_entry_names(
        &published.lane.ancestor_records,
        MAX_COMPONENT_ANCESTOR_SHARDS + 1,
    )
    .map_err(tx)?;
    let mut prefix = 0;
    while prefix < authority.shard_count()
        && bucket_names.contains(&component_ancestor_bucket_name(prefix).map_err(tx)?)
        && record_names.contains(&component_ancestor_record_file_name(prefix).map_err(tx)?)
    {
        prefix += 1;
    }
    let expected_buckets = (0..prefix)
        .map(component_ancestor_bucket_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    let expected_records = (0..prefix)
        .map(component_ancestor_record_file_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    if bucket_names != expected_buckets || record_names != expected_records {
        return Err(ComponentTransactionError);
    }
    for shard_index in 0..prefix {
        read_ancestor_journal(published, authority, shard_index)?;
    }
    Ok(prefix)
}

fn admit_ancestor_recovery(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
    allow_unjournaled_cleanup: bool,
) -> Result<AncestorRecoveryPlan, ComponentTransactionError> {
    let record_names = exact_entry_names(
        &published.lane.ancestor_records,
        MAX_COMPONENT_ANCESTOR_SHARDS + 1,
    )
    .map_err(tx)?;
    let durable_shards = exact_prefix_len(
        &record_names,
        authority.shard_count(),
        component_ancestor_record_file_name,
    )?;
    let mut bucket_names = exact_entry_names(
        &published.lane.ancestor_staging,
        MAX_COMPONENT_ANCESTOR_SHARDS + 2,
    )
    .map_err(tx)?;
    let parked_bucket = admit_empty_recovery_park(
        &published.lane.ancestor_staging,
        &mut bucket_names,
        COMPONENT_BUCKET_PARK_A,
        COMPONENT_BUCKET_PARK_B,
    )?;
    let bucket_prefix = exact_prefix_len(
        &bucket_names,
        authority.shard_count(),
        component_ancestor_bucket_name,
    )?;
    if bucket_prefix < durable_shards || bucket_prefix > durable_shards.saturating_add(1) {
        return Err(ComponentTransactionError);
    }
    if parked_bucket.is_some()
        && (!allow_unjournaled_cleanup
            || durable_shards >= authority.shard_count()
            || bucket_prefix != durable_shards)
    {
        return Err(ComponentTransactionError);
    }
    let mut canonical_records = 0_usize;
    let mut staged_suffix_started = false;
    let mut journaled_records = 0_usize;
    for shard_index in 0..durable_shards {
        let shard = read_ancestor_journal(published, authority, shard_index)?;
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = published
            .lane
            .ancestor_staging
            .open_child(&bucket_name)
            .map_err(tx)?;
        let mut expected_staged = BTreeSet::new();
        for record in shard.records() {
            journaled_records = journaled_records
                .checked_add(1)
                .ok_or(ComponentTransactionError)?;
            let slot_name =
                component_slot_name(record.ordinal() % COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
                    .map_err(tx)?;
            let canonical = open_canonical_ancestor(published, record.target())?;
            let staged = if bucket
                .has_portably_exact_child_name(&slot_name)
                .map_err(tx)?
            {
                Some(bucket.open_child(&slot_name).map_err(tx)?)
            } else {
                None
            };
            match (canonical, staged) {
                (Some(canonical), None)
                    if !staged_suffix_started
                        && record.matches_identity(canonical.identity().map_err(tx)?) =>
                {
                    canonical_records = canonical_records
                        .checked_add(1)
                        .ok_or(ComponentTransactionError)?;
                }
                (None, Some(staged))
                    if record.matches_identity(staged.identity().map_err(tx)?)
                        && exact_entry_names(&staged, 1).map_err(tx)?.is_empty() =>
                {
                    staged_suffix_started = true;
                    expected_staged.insert(slot_name);
                }
                _ => return Err(ComponentTransactionError),
            }
        }
        if exact_entry_names(&bucket, COMPONENT_ANCESTOR_RECORDS_PER_SHARD + 1).map_err(tx)?
            != expected_staged
        {
            return Err(ComponentTransactionError);
        }
    }
    for target in targets.iter().skip(journaled_records) {
        if open_canonical_ancestor(published, target)?.is_some() {
            return Err(ComponentTransactionError);
        }
    }
    if bucket_prefix == durable_shards + 1 {
        if !allow_unjournaled_cleanup {
            return Err(ComponentTransactionError);
        }
        cleanup_unjournaled_ancestor_bucket(published, authority, targets, durable_shards)?;
    }
    if let Some(parked) = parked_bucket {
        finish_empty_recovery_park(&published.lane.ancestor_staging, parked)?;
        sync_transaction_roots(published)?;
    }
    sync_transaction_roots(published)?;
    Ok(AncestorRecoveryPlan {
        durable_shards,
        canonical_records,
    })
}

fn exact_prefix_len(
    names: &BTreeSet<String>,
    maximum: usize,
    expected_name: impl Fn(usize) -> Result<String, ComponentEffectsError>,
) -> Result<usize, ComponentTransactionError> {
    if names.len() > maximum {
        return Err(ComponentTransactionError);
    }
    let expected = (0..names.len())
        .map(expected_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    if *names != expected {
        return Err(ComponentTransactionError);
    }
    Ok(names.len())
}

fn admit_empty_recovery_park(
    parent: &ManagedDir,
    names: &mut BTreeSet<String>,
    first: &'static str,
    second: &'static str,
) -> Result<Option<EmptyRecoveryPark>, ComponentTransactionError> {
    let (name, alternate) = match (names.remove(first), names.remove(second)) {
        (false, false) => return Ok(None),
        (true, false) => (first, second),
        (false, true) => (second, first),
        (true, true) => return Err(ComponentTransactionError),
    };
    let directory = parent.open_child(name).map_err(tx)?;
    if !exact_entry_names(&directory, 1).map_err(tx)?.is_empty() {
        return Err(ComponentTransactionError);
    }
    Ok(Some(EmptyRecoveryPark {
        name,
        alternate,
        directory,
    }))
}

fn finish_empty_recovery_park(
    parent: &ManagedDir,
    parked: EmptyRecoveryPark,
) -> Result<(), ComponentTransactionError> {
    if parent
        .remove_empty_child_guarded(parked.name, parked.alternate, parked.directory)
        .map_err(tx)?
        != ManagedEmptyChildRemoval::Removed
    {
        return Err(ComponentTransactionError);
    }
    parent.sync().map_err(tx)
}

fn cleanup_unjournaled_ancestor_bucket(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
    shard_index: usize,
) -> Result<(), ComponentTransactionError> {
    if shard_index >= authority.shard_count() {
        return Err(ComponentTransactionError);
    }
    let first = shard_index
        .checked_mul(COMPONENT_ANCESTOR_RECORDS_PER_SHARD)
        .ok_or(ComponentTransactionError)?;
    let expected_slots = (targets.len() - first).min(COMPONENT_ANCESTOR_RECORDS_PER_SHARD);
    let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
    let bucket = published
        .lane
        .ancestor_staging
        .open_child(&bucket_name)
        .map_err(tx)?;
    let mut names = exact_entry_names(&bucket, expected_slots + 2).map_err(tx)?;
    let parked_slot = admit_empty_recovery_park(
        &bucket,
        &mut names,
        ANCESTOR_SLOT_PARK_A,
        ANCESTOR_SLOT_PARK_B,
    )?;
    let slot_prefix = exact_prefix_len(&names, expected_slots, component_slot_name)?;
    let mut slots = Vec::new();
    slots.try_reserve_exact(slot_prefix).map_err(tx)?;
    for row_in_shard in 0..slot_prefix {
        let ordinal = first
            .checked_add(row_in_shard)
            .ok_or(ComponentTransactionError)?;
        if open_canonical_ancestor(
            published,
            targets.get(ordinal).ok_or(ComponentTransactionError)?,
        )?
        .is_some()
        {
            return Err(ComponentTransactionError);
        }
        let slot_name = component_slot_name(row_in_shard).map_err(tx)?;
        let slot = bucket.open_child(&slot_name).map_err(tx)?;
        if !exact_entry_names(&slot, 1).map_err(tx)?.is_empty() {
            return Err(ComponentTransactionError);
        }
        slots.push(slot);
    }
    if let Some(parked) = parked_slot {
        finish_empty_recovery_park(&bucket, parked)?;
    }
    cleanup_unjournaled_bucket(
        &published.lane.ancestor_staging,
        &bucket_name,
        bucket,
        slots,
    )?;
    sync_transaction_roots(published)
}

fn open_canonical_ancestor(
    published: &ComponentIntentPublished,
    target: &ComponentCreatedAncestor,
) -> Result<Option<ManagedDir>, ComponentTransactionError> {
    published.lease.revalidate().map_err(tx)?;
    let root = published.lease.root();
    let component_root = component_lane_name(published.lane.component);
    let result = (|| -> Result<Option<ManagedDir>, ComponentTransactionError> {
        match target {
            ComponentCreatedAncestor::ComponentRoot => {
                if root
                    .has_portably_exact_child_name(component_root)
                    .map_err(tx)?
                {
                    Ok(Some(root.open_child(component_root).map_err(tx)?))
                } else {
                    Ok(None)
                }
            }
            ComponentCreatedAncestor::Relative(path) => {
                if !root
                    .has_portably_exact_child_name(component_root)
                    .map_err(tx)?
                {
                    return Ok(None);
                }
                let mut current = root.open_child(component_root).map_err(tx)?;
                for segment in path.as_str().split('/') {
                    if !current.has_portably_exact_child_name(segment).map_err(tx)? {
                        return Ok(None);
                    }
                    current = current.open_child(segment).map_err(tx)?;
                }
                Ok(Some(current))
            }
        }
    })()?;
    published.lease.revalidate().map_err(tx)?;
    Ok(result)
}

fn read_ancestor_journal(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    shard_index: usize,
) -> Result<
    crate::managed_component_ancestor_journal::ComponentAncestorJournalShard,
    ComponentTransactionError,
> {
    let name = component_ancestor_record_file_name(shard_index).map_err(tx)?;
    let guard = published
        .lane
        .ancestor_records
        .inspect_regular_file(&name)
        .map_err(tx)?
        .ok_or(ComponentTransactionError)?;
    let bytes = published
        .lane
        .ancestor_records
        .read_guarded_file_bounded(
            &name,
            &guard,
            MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES as u64,
        )
        .map_err(tx)?;
    let shard = authority.decode_shard(&bytes).map_err(tx)?;
    if shard.shard_index() != shard_index {
        return Err(ComponentTransactionError);
    }
    Ok(shard)
}

fn canonical_ancestor_parent(
    published: &ComponentIntentPublished,
    target: &ComponentCreatedAncestor,
) -> Result<(ManagedDir, String), ComponentTransactionError> {
    published.lease.revalidate().map_err(tx)?;
    let result = match target {
        ComponentCreatedAncestor::ComponentRoot => {
            let root = published.lease.root();
            let name = component_lane_name(published.lane.component).to_string();
            let _ = root.has_portably_exact_child_name(&name).map_err(tx)?;
            Ok((root.clone(), name))
        }
        ComponentCreatedAncestor::Relative(path) => {
            let mut segments = path.as_str().split('/').collect::<Vec<_>>();
            let name = segments.pop().ok_or(ComponentTransactionError)?.to_string();
            let mut parent = published
                .lease
                .root()
                .open_child(component_lane_name(published.lane.component))
                .map_err(tx)?;
            for segment in segments {
                if !parent.has_portably_exact_child_name(segment).map_err(tx)? {
                    return Err(ComponentTransactionError);
                }
                parent = parent.open_child(segment).map_err(tx)?;
            }
            let _ = parent.has_portably_exact_child_name(&name).map_err(tx)?;
            Ok((parent, name))
        }
    };
    published.lease.revalidate().map_err(tx)?;
    result
}

fn postcheck(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
    expected: ComponentRecoveryDecision,
) -> Result<(), ComponentTransactionError> {
    postcheck_with_outcome(published, authority, targets, expected, false)
}

fn postcheck_with_outcome(
    published: &ComponentIntentPublished,
    authority: &ComponentAncestorJournalAuthority<'_>,
    targets: &[ComponentCreatedAncestor],
    expected: ComponentRecoveryDecision,
    outcome_present: bool,
) -> Result<(), ComponentTransactionError> {
    validate_published_marker(published)?;
    validate_terminal_topology(published, outcome_present)?;
    observe_all_rows(
        published,
        expected,
        expected == ComponentRecoveryDecision::Rollback,
    )?;
    match expected {
        ComponentRecoveryDecision::Commit => {
            prove_ancestors_committed(published, authority, targets)?
        }
        ComponentRecoveryDecision::Rollback => {
            prove_ancestors_rolled_back(published, authority, targets)?
        }
    }
    sync_transaction_roots(published)
}

fn validate_published_marker(
    published: &ComponentIntentPublished,
) -> Result<(), ComponentTransactionError> {
    if published
        .lane
        .lane
        .read_guarded_file_bounded(
            COMPONENT_INTENT_FILE,
            &published.intent_guard,
            MAX_COMPONENT_INTENT_BYTES as u64,
        )
        .map_err(tx)?
        != published.encoded_intent
    {
        return Err(ComponentTransactionError);
    }
    published.lease.revalidate().map_err(tx)
}

fn publish_outcome(
    published: &ComponentIntentPublished,
    terminal: ComponentTerminalOutcome,
    effect: ComponentRollbackEffect,
    fault: ComponentExecutionFault,
) -> Result<ManagedFileGuard, OutcomePublicationFailure> {
    let outcome = ComponentOutcomeRecord::for_intent(&published.encoded_intent, terminal, effect)
        .map_err(|_| OutcomePublicationFailure::BeforePromotion)?;
    let encoded = encode_component_outcome(&outcome)
        .map_err(|_| OutcomePublicationFailure::BeforePromotion)?;
    #[cfg(test)]
    let write = if fault == ComponentExecutionFault::OutcomePromotionAttempted {
        published.lane.lane.write_new_exact_retained_with_fault(
            COMPONENT_OUTCOME_FILE,
            &encoded,
            crate::managed_fs::ManagedCreateOnlyWriteFault::AfterPromotion,
        )
    } else {
        published
            .lane
            .lane
            .write_new_exact_retained(COMPONENT_OUTCOME_FILE, &encoded)
    };
    #[cfg(not(test))]
    let write = {
        let _ = fault;
        published
            .lane
            .lane
            .write_new_exact_retained(COMPONENT_OUTCOME_FILE, &encoded)
    };
    let guard = match write {
        Ok(guard) => guard,
        Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_)) => {
            return Err(OutcomePublicationFailure::BeforePromotion);
        }
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard, .. }) => {
            return Err(OutcomePublicationFailure::PromotionAttempted(final_guard));
        }
    };
    let validation = (|| {
        if published
            .lane
            .lane
            .read_guarded_file_bounded(COMPONENT_OUTCOME_FILE, &guard, encoded.len() as u64)
            .map_err(tx)?
            != encoded
        {
            return Err(ComponentTransactionError);
        }
        published.lane.lane.sync().map_err(tx)?;
        published.lease.publication_directory().sync().map_err(tx)?;
        published.lease.root().sync().map_err(tx)?;
        published.lease.revalidate().map_err(tx)?;
        validate_published_marker(published)?;
        if published
            .lane
            .lane
            .read_guarded_file_bounded(COMPONENT_OUTCOME_FILE, &guard, encoded.len() as u64)
            .map_err(tx)?
            != encoded
        {
            return Err(ComponentTransactionError);
        }
        validate_terminal_topology(published, true)
    })();
    match validation {
        Ok(()) => Ok(guard),
        Err(_) => Err(OutcomePublicationFailure::PromotionAttempted(Some(guard))),
    }
}

fn validate_terminal_topology(
    published: &ComponentIntentPublished,
    outcome_present: bool,
) -> Result<(), ComponentTransactionError> {
    published.lease.revalidate().map_err(tx)?;
    let lane = published
        .lease
        .publication_directory()
        .open_child(component_lane_name(published.lane.component))
        .map_err(tx)?;
    let table = lane.open_child(COMPONENT_TABLE_DIRECTORY).map_err(tx)?;
    let staging = lane.open_child(COMPONENT_STAGING_DIRECTORY).map_err(tx)?;
    let quarantine = lane
        .open_child(COMPONENT_QUARANTINE_DIRECTORY)
        .map_err(tx)?;
    let ancestors = lane.open_child(COMPONENT_ANCESTORS_DIRECTORY).map_err(tx)?;
    let records = ancestors
        .open_child(COMPONENT_ANCESTOR_RECORDS_DIRECTORY)
        .map_err(tx)?;
    let ancestor_staging = ancestors
        .open_child(COMPONENT_ANCESTOR_STAGING_DIRECTORY)
        .map_err(tx)?;
    if lane.identity().map_err(tx)? != published.lane.lane.identity().map_err(tx)?
        || table.identity().map_err(tx)? != published.lane.table.identity().map_err(tx)?
        || staging.identity().map_err(tx)? != published.lane.staging.identity().map_err(tx)?
        || quarantine.identity().map_err(tx)? != published.lane.quarantine.identity().map_err(tx)?
        || ancestors.identity().map_err(tx)? != published.lane.ancestors.identity().map_err(tx)?
        || records.identity().map_err(tx)?
            != published.lane.ancestor_records.identity().map_err(tx)?
        || ancestor_staging.identity().map_err(tx)?
            != published.lane.ancestor_staging.identity().map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }

    let mut expected_lane = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_INTENT_FILE.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    if outcome_present {
        expected_lane.insert(COMPONENT_OUTCOME_FILE.to_string());
    }
    let expected_ancestor_children = BTreeSet::from([
        COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
        COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
    ]);
    let expected_table = (0..published.manifest.shards.len())
        .map(component_table_file_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    let expected_buckets = (0..published.manifest.shards.len())
        .map(component_bucket_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    if exact_entry_names(&lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)? != expected_lane
        || exact_entry_names(&ancestors, 3).map_err(tx)? != expected_ancestor_children
        || exact_entry_names(&table, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)? != expected_table
        || exact_entry_names(&staging, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)?
            != expected_buckets
        || exact_entry_names(&quarantine, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)?
            != expected_buckets
    {
        return Err(ComponentTransactionError);
    }
    published.lease.revalidate().map_err(tx)
}

fn read_table_shard_bytes(
    published: &ComponentIntentPublished,
    shard_index: usize,
) -> Result<Vec<u8>, ComponentTransactionError> {
    let descriptor = published
        .manifest
        .shards
        .get(shard_index)
        .ok_or(ComponentTransactionError)?;
    let name = component_table_file_name(shard_index).map_err(tx)?;
    let guard = published
        .lane
        .table
        .inspect_regular_file(&name)
        .map_err(tx)?
        .ok_or(ComponentTransactionError)?;
    if guard.size() != u64::from(descriptor.byte_len)
        || guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64
    {
        return Err(ComponentTransactionError);
    }
    let bytes = published
        .lane
        .table
        .read_guarded_file_bounded(&name, &guard, MAX_COMPONENT_TABLE_SHARD_BYTES as u64)
        .map_err(tx)?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != descriptor.sha256 {
        return Err(ComponentTransactionError);
    }
    Ok(bytes)
}

fn read_authenticated_table_shard(
    published: &ComponentIntentPublished,
    shard_index: usize,
) -> Result<ComponentTableShard, ComponentTransactionError> {
    let descriptor = published
        .manifest
        .shards
        .get(shard_index)
        .ok_or(ComponentTransactionError)?;
    let shard = decode_component_table_shard(&read_table_shard_bytes(published, shard_index)?)
        .map_err(tx)?;
    if usize::try_from(shard.shard_index).map_err(tx)? != shard_index
        || shard.shard_index != descriptor.shard_index
        || shard.first_row != descriptor.first_row
        || usize::try_from(descriptor.row_count).map_err(tx)? != shard.rows.len()
        || shard.total_rows != published.manifest.total_rows
        || shard.component != published.manifest.component
        || shard.transaction_nonce != published.manifest.transaction_nonce
        || shard.root_binding_sha256 != published.manifest.root_binding_sha256
    {
        return Err(ComponentTransactionError);
    }
    Ok(shard)
}

fn sync_file_move(
    published: &ComponentIntentPublished,
    source: &ManagedDir,
    source_name: &str,
    destination: &ManagedDir,
    destination_name: &str,
    guard: &ManagedFileGuard,
    expected_size: u64,
    expected_sha1: [u8; 20],
) -> Result<(), ComponentTransactionError> {
    source.sync().map_err(tx)?;
    destination.sync().map_err(tx)?;
    sync_transaction_roots(published)?;
    if source.file_guard_matches(source_name, guard).map_err(tx)?
        || !destination
            .file_guard_matches(destination_name, guard)
            .map_err(tx)?
        || guard.size() != expected_size
        || destination
            .sha1_guarded_file_bytes(destination_name, guard, MAX_TIER2_ARTIFACT_BYTES)
            .map_err(tx)?
            != expected_sha1
    {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn sync_transaction_roots(
    published: &ComponentIntentPublished,
) -> Result<(), ComponentTransactionError> {
    published.lane.lane.sync().map_err(tx)?;
    published.lease.publication_directory().sync().map_err(tx)?;
    published.lease.root().sync().map_err(tx)?;
    published.lease.revalidate().map_err(tx)
}

fn finish_disposition(
    shared: &Mutex<Option<ComponentIntentPublished>>,
    disposition: BlockingDisposition,
) -> ComponentExecutionResult {
    let context = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("component terminal owner must retain its context");
    match disposition {
        BlockingDisposition::NoTransaction | BlockingDisposition::RetryIntent => {
            unreachable!("live execution cannot return a recovery admission disposition")
        }
        BlockingDisposition::Committed(outcome_guard) => {
            ComponentExecutionResult::Committed(ComponentCommitReceipt {
                context,
                outcome_guard,
            })
        }
        BlockingDisposition::RolledBack(outcome_guard) => {
            ComponentExecutionResult::RolledBack(ComponentRollbackReceipt {
                context,
                outcome_guard,
            })
        }
        BlockingDisposition::RecoveryRequired(outcome_guard) => {
            ComponentExecutionResult::RecoveryRequired(ComponentRecoveryRequired {
                authority: ComponentRecoveryAuthority::Published {
                    context,
                    outcome_guard,
                },
            })
        }
    }
}

fn finish_recovery_disposition(
    shared: &Mutex<Option<ComponentRecoveryAuthority>>,
    disposition: BlockingDisposition,
) -> RecoveryOwnerResult {
    let authority = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("component recovery owner must retain its authority");
    match (disposition, authority) {
        (BlockingDisposition::NoTransaction, ComponentRecoveryAuthority::Restart { lease, .. }) => {
            RecoveryOwnerResult::NoTransaction(lease)
        }
        (
            BlockingDisposition::RetryIntent,
            ComponentRecoveryAuthority::IntentPromotionAttempted(
                ComponentIntentPublishFailure::PromotionAttempted { candidate, .. },
            ),
        ) => RecoveryOwnerResult::RetryIntent(*candidate),
        (
            BlockingDisposition::Committed(outcome_guard),
            ComponentRecoveryAuthority::Published { context, .. },
        ) => RecoveryOwnerResult::Transaction(ComponentExecutionResult::Committed(
            ComponentCommitReceipt {
                context,
                outcome_guard,
            },
        )),
        (
            BlockingDisposition::RolledBack(outcome_guard),
            ComponentRecoveryAuthority::Published { context, .. },
        ) => RecoveryOwnerResult::Transaction(ComponentExecutionResult::RolledBack(
            ComponentRollbackReceipt {
                context,
                outcome_guard,
            },
        )),
        (BlockingDisposition::RecoveryRequired(outcome_guard), authority) => {
            let authority = match authority {
                ComponentRecoveryAuthority::Published { context, .. } => {
                    ComponentRecoveryAuthority::Published {
                        context,
                        outcome_guard,
                    }
                }
                authority => authority,
            };
            RecoveryOwnerResult::Transaction(ComponentExecutionResult::RecoveryRequired(
                ComponentRecoveryRequired { authority },
            ))
        }
        (_, authority) => RecoveryOwnerResult::Transaction(
            ComponentExecutionResult::RecoveryRequired(ComponentRecoveryRequired { authority }),
        ),
    }
}

fn directory_move_error(_: ManagedDirectoryMoveFailure) -> ComponentTransactionError {
    ComponentTransactionError
}

fn tx<T>(_: T) -> ComponentTransactionError {
    ComponentTransactionError
}
