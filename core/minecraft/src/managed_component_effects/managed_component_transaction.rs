use super::*;
use crate::managed_component_ancestor_journal::{
    COMPONENT_ANCESTOR_RECORDS_PER_SHARD, ComponentAncestorJournalAuthority,
    ComponentAncestorJournalRecord, MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES,
};
use crate::managed_component_publication::{
    COMPONENT_OUTCOME_BYTES, COMPONENT_OUTCOME_FILE, COMPONENT_SETTLEMENT_FILE,
    ComponentObservedCanonical, ComponentOutcomeRecord, ComponentRecoveryDecision,
    ComponentRecoveryEntryState, ComponentRecoveryObservation, ComponentRecoveryPlan,
    ComponentRecoveryPlanner, ComponentRollbackEffect, ComponentSettlementRecord,
    ComponentTerminalOutcome, MAX_COMPONENT_SETTLEMENT_BYTES, decode_component_outcome,
    decode_component_settlement, encode_component_outcome, encode_component_settlement,
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
    Committed(ComponentTransactionReceipt),
    RolledBack(ComponentTransactionReceipt),
    RecoveryRequired(ComponentRecoveryRequired),
}

pub(crate) enum ComponentStartupRecoveryResult {
    NoTransaction(ManagedRootPublicationLease),
    Settled(ComponentSettledOutcome),
    Transaction(ComponentExecutionResult),
}

pub(crate) enum ComponentIntentPublicationRecovery {
    Retry(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

pub(crate) enum ComponentRecoveryRetryResult {
    NoTransaction(ManagedRootPublicationLease),
    Settled(ComponentSettledOutcome),
    RetryIntent(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

pub(crate) enum ComponentSettlementResult {
    Settled(ComponentSettledOutcome),
    Retry(ComponentSettlementRetry),
}

pub(crate) enum ComponentSettledOutcome {
    Committed(ManagedRootPublicationLease),
    RolledBack {
        lease: ManagedRootPublicationLease,
        effect: ComponentRollbackEffect,
    },
}

pub(crate) struct ComponentTransactionReceipt {
    context: ComponentIntentPublished,
    outcome_guard: ManagedFileGuard,
    terminal: ComponentTerminalOutcome,
}

pub(crate) struct ComponentSettlementRetry {
    authority: ComponentSettlementAuthority,
}

pub(crate) struct ComponentRecoveryRequired {
    authority: ComponentRecoveryAuthority,
}

#[cfg(test)]
impl ComponentTransactionReceipt {
    pub(crate) fn into_restart_seed(self) -> (ManagedRootPublicationLease, ManagedComponentKind) {
        let component = self.context.manifest.component;
        drop(self.outcome_guard);
        (self.context.lease, component)
    }
}

#[cfg(test)]
impl ComponentRecoveryRequired {
    pub(crate) fn into_restart_seed(self) -> (ManagedRootPublicationLease, ManagedComponentKind) {
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

struct ComponentSettlementAuthority {
    context: ComponentIntentPublished,
    outcome_guard: ManagedFileGuard,
    terminal: ComponentTerminalOutcome,
    outcome: Option<ComponentOutcomeRecord>,
    settlement_identity: Option<ManagedFileIdentity>,
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
    Settled(ComponentOutcomeRecord),
    Committed(ManagedFileGuard),
    RolledBack(ManagedFileGuard),
    RecoveryRequired(Option<ManagedFileGuard>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SettlementDisposition {
    Settled,
    Retry,
}

enum RecoveryOwnerResult {
    NoTransaction(ManagedRootPublicationLease),
    Settled(ComponentSettledOutcome),
    RetryIntent(ComponentIntentCandidate),
    Transaction(ComponentExecutionResult),
}

enum RecoveryNormalization {
    Published,
    NoTransaction,
    Settled(ComponentOutcomeRecord),
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

struct SettlementAncestorPlan {
    record_count: usize,
    created_ancestors: Vec<ComponentCreatedAncestor>,
}

struct SettlementRowsPlan {
    table_count: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ComponentExecutionFault {
    None,
    AfterFirstRow,
    CrashAfterFirstRow,
    CrashAfterFirstReplacementQuarantine,
    CrashAfterFirstAncestor,
    CrashBeforeOutcome,
    OutcomePromotionAttempted,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ComponentSettlementFault {
    None,
    AfterSettlementPromotion,
    AfterAncestorBucket,
    AfterAncestorRecord,
    AfterStagingBucket,
    AfterQuarantineBucket,
    AfterTableShard,
    AfterOutcomeRemoval,
    AfterIntentRemoval,
    AfterSettlementRemoval,
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
        RecoveryOwnerResult::Settled(outcome) => ComponentStartupRecoveryResult::Settled(outcome),
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
            RecoveryOwnerResult::Settled(_) => {
                unreachable!("attempted publication recovery cannot settle a terminal transaction")
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
        RecoveryOwnerResult::Settled(outcome) => ComponentRecoveryRetryResult::Settled(outcome),
        RecoveryOwnerResult::Transaction(result) => {
            ComponentRecoveryRetryResult::Transaction(result)
        }
        RecoveryOwnerResult::RetryIntent(candidate) => {
            ComponentRecoveryRetryResult::RetryIntent(candidate)
        }
    }
}

pub(crate) async fn settle_component_transaction(
    receipt: ComponentTransactionReceipt,
) -> ComponentSettlementResult {
    settle_component_transaction_inner(receipt, ComponentSettlementFault::None).await
}

pub(crate) async fn retry_component_settlement(
    retry: ComponentSettlementRetry,
) -> ComponentSettlementResult {
    run_component_settlement(retry.authority, ComponentSettlementFault::None).await
}

#[cfg(test)]
pub(crate) async fn settle_component_transaction_with_fault(
    receipt: ComponentTransactionReceipt,
    fault: ComponentSettlementFault,
) -> ComponentSettlementResult {
    settle_component_transaction_inner(receipt, fault).await
}

async fn settle_component_transaction_inner(
    receipt: ComponentTransactionReceipt,
    fault: ComponentSettlementFault,
) -> ComponentSettlementResult {
    let ComponentTransactionReceipt {
        context,
        outcome_guard,
        terminal,
    } = receipt;
    run_component_settlement(
        ComponentSettlementAuthority {
            context,
            outcome_guard,
            terminal,
            outcome: None,
            settlement_identity: None,
        },
        fault,
    )
    .await
}

async fn run_component_settlement(
    authority: ComponentSettlementAuthority,
    fault: ComponentSettlementFault,
) -> ComponentSettlementResult {
    let shared = Arc::new(Mutex::new(Some(authority)));
    let owner_authority = Arc::clone(&shared);
    let owner = tokio::spawn(async move {
        let worker_authority = Arc::clone(&owner_authority);
        let disposition = match run_publication_blocking(move || {
            let mut slot = worker_authority
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(authority) = slot.as_mut() else {
                return SettlementDisposition::Retry;
            };
            catch_unwind(AssertUnwindSafe(|| {
                settle_component_transaction_blocking(authority, fault)
            }))
            .unwrap_or(SettlementDisposition::Retry)
        })
        .await
        {
            Ok(disposition) => disposition,
            Err(_) => SettlementDisposition::Retry,
        };
        finish_settlement_disposition(&owner_authority, disposition)
    });
    match owner.await {
        Ok(result) => result,
        Err(_) => finish_settlement_disposition(&shared, SettlementDisposition::Retry),
    }
}

#[cfg(test)]
pub(crate) async fn execute_component_intent_with_fault(
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
                Ok(Ok(RecoveryNormalization::Settled(outcome))) => {
                    return BlockingDisposition::Settled(outcome);
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
            if let Some(outcome) = recover_restart_component_settlement(lease, *component)? {
                return Ok(RecoveryNormalization::Settled(outcome));
            }
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
            Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard: _ }) => {
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
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
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

fn settle_component_transaction_blocking(
    authority: &mut ComponentSettlementAuthority,
    fault: ComponentSettlementFault,
) -> SettlementDisposition {
    match settle_component_transaction_attempt(authority, fault) {
        Ok(()) => SettlementDisposition::Settled,
        Err(_) => SettlementDisposition::Retry,
    }
}

fn settle_component_transaction_attempt(
    authority: &mut ComponentSettlementAuthority,
    fault: ComponentSettlementFault,
) -> Result<(), ComponentTransactionError> {
    authority.context.lease.revalidate().map_err(tx)?;
    if let Some((settlement, guard)) = read_component_settlement(&authority.context.lane)? {
        if authority
            .settlement_identity
            .is_some_and(|identity| identity != guard.identity())
        {
            return Err(ComponentTransactionError);
        }
        authority.settlement_identity = Some(guard.identity());
        validate_live_settlement_authority(authority, &settlement)?;
        authority.outcome = Some(settlement.outcome.clone());
        cleanup_component_settlement(
            &authority.context.lane,
            &authority.context.lease,
            &settlement,
            &guard,
            fault,
        )?;
        return Ok(());
    }

    let lane_names =
        exact_entry_names(&authority.context.lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1)
            .map_err(tx)?;
    let intent_present = lane_names.contains(COMPONENT_INTENT_FILE);
    let outcome_present = lane_names.contains(COMPONENT_OUTCOME_FILE);
    if !intent_present && !outcome_present {
        let outcome = authority
            .outcome
            .as_ref()
            .ok_or(ComponentTransactionError)?;
        validate_marker_free_settlement_shape(
            &authority.context.lane,
            &authority.context.lease,
            outcome,
        )?;
        sync_component_lane_roots(&authority.context.lane, &authority.context.lease)?;
        return Ok(());
    }
    if !intent_present || !outcome_present {
        return Err(ComponentTransactionError);
    }

    let (outcome, observed_outcome_guard) =
        read_recovery_outcome(&authority.context, Some(&authority.outcome_guard))?
            .ok_or(ComponentTransactionError)?;
    if observed_outcome_guard.identity() != authority.outcome_guard.identity()
        || outcome.terminal != authority.terminal
    {
        return Err(ComponentTransactionError);
    }
    prove_terminal_for_settlement(&authority.context, &outcome)?;
    authority.outcome = Some(outcome.clone());
    let encoded =
        encode_component_settlement(&outcome, &authority.context.encoded_intent).map_err(tx)?;
    let write = authority
        .context
        .lane
        .lane
        .write_new_exact_retained(COMPONENT_SETTLEMENT_FILE, &encoded);
    let settlement_guard = match write {
        Ok(guard) => guard,
        Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_)) => {
            return Err(ComponentTransactionError);
        }
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
            authority.settlement_identity = final_guard.as_ref().map(ManagedFileGuard::identity);
            return Err(ComponentTransactionError);
        }
    };
    authority.settlement_identity = Some(settlement_guard.identity());
    let durable = authority
        .context
        .lane
        .lane
        .read_guarded_file_bounded(
            COMPONENT_SETTLEMENT_FILE,
            &settlement_guard,
            MAX_COMPONENT_SETTLEMENT_BYTES as u64,
        )
        .map_err(tx)?;
    if durable != encoded {
        return Err(ComponentTransactionError);
    }
    let settlement = decode_component_settlement(&durable).map_err(tx)?;
    validate_live_settlement_authority(authority, &settlement)?;
    sync_component_lane_roots(&authority.context.lane, &authority.context.lease)?;
    if fault == ComponentSettlementFault::AfterSettlementPromotion {
        return Err(ComponentTransactionError);
    }
    cleanup_component_settlement(
        &authority.context.lane,
        &authority.context.lease,
        &settlement,
        &settlement_guard,
        fault,
    )
}

fn prove_terminal_for_settlement(
    published: &ComponentIntentPublished,
    outcome: &ComponentOutcomeRecord,
) -> Result<(), ComponentTransactionError> {
    outcome
        .binds_intent(&published.manifest, &published.encoded_intent)
        .map_err(tx)?;
    let summary = validate_published_and_replay(published, true)?;
    let authority = ComponentAncestorJournalAuthority::new(
        &published.encoded_intent,
        &summary.created_ancestors,
    )
    .map_err(tx)?;
    postcheck_with_outcome(
        published,
        &authority,
        &summary.created_ancestors,
        match outcome.terminal {
            ComponentTerminalOutcome::Committed => ComponentRecoveryDecision::Commit,
            ComponentTerminalOutcome::RolledBack => ComponentRecoveryDecision::Rollback,
        },
        true,
    )
}

fn validate_live_settlement_authority(
    authority: &ComponentSettlementAuthority,
    settlement: &ComponentSettlementRecord,
) -> Result<(), ComponentTransactionError> {
    if settlement.intent != authority.context.manifest
        || settlement.encoded_intent != authority.context.encoded_intent
        || settlement.outcome.terminal != authority.terminal
        || settlement.outcome.component != authority.context.lane.component
        || settlement
            .outcome
            .binds_intent(
                &authority.context.manifest,
                &authority.context.encoded_intent,
            )
            .is_err()
    {
        return Err(ComponentTransactionError);
    }
    if let Some(intent) = authority
        .context
        .lane
        .lane
        .inspect_regular_file(COMPONENT_INTENT_FILE)
        .map_err(tx)?
        && intent.identity() != authority.context.intent_guard.identity()
    {
        return Err(ComponentTransactionError);
    }
    if let Some(outcome) = authority
        .context
        .lane
        .lane
        .inspect_regular_file(COMPONENT_OUTCOME_FILE)
        .map_err(tx)?
        && outcome.identity() != authority.outcome_guard.identity()
    {
        return Err(ComponentTransactionError);
    }
    authority.context.lease.revalidate().map_err(tx)?;
    if settlement.intent.root_binding_sha256
        != component_root_binding_sha256(authority.context.lease.root()).map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn read_component_settlement(
    lane: &ComponentLane,
) -> Result<Option<(ComponentSettlementRecord, ManagedFileGuard)>, ComponentTransactionError> {
    let Some(guard) = lane
        .lane
        .inspect_regular_file(COMPONENT_SETTLEMENT_FILE)
        .map_err(tx)?
    else {
        return Ok(None);
    };
    if guard.size() > MAX_COMPONENT_SETTLEMENT_BYTES as u64 {
        return Err(ComponentTransactionError);
    }
    let bytes = lane
        .lane
        .read_guarded_file_bounded(
            COMPONENT_SETTLEMENT_FILE,
            &guard,
            MAX_COMPONENT_SETTLEMENT_BYTES as u64,
        )
        .map_err(tx)?;
    let settlement = decode_component_settlement(&bytes).map_err(tx)?;
    Ok(Some((settlement, guard)))
}

fn recover_restart_component_settlement(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<Option<ComponentOutcomeRecord>, ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let publication = lease.publication_directory();
    let lane_name = component_lane_name(component);
    if !publication
        .has_portably_exact_child_name(lane_name)
        .map_err(tx)?
    {
        return Ok(None);
    }
    let marker_lane = publication.open_child(lane_name).map_err(tx)?;
    if !marker_lane
        .has_portably_exact_child_name(COMPONENT_SETTLEMENT_FILE)
        .map_err(tx)?
    {
        return Ok(None);
    }
    drop(marker_lane);
    let lane = open_component_settlement_lane(lease, component)?;
    let (settlement, guard) = read_component_settlement(&lane)?.ok_or(ComponentTransactionError)?;
    if settlement.intent.component != component
        || settlement.intent.root_binding_sha256
            != component_root_binding_sha256(lease.root()).map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    cleanup_component_settlement(
        &lane,
        lease,
        &settlement,
        &guard,
        ComponentSettlementFault::None,
    )?;
    Ok(Some(settlement.outcome))
}

fn open_component_settlement_lane(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<ComponentLane, ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let lane = lease
        .publication_directory()
        .open_child(component_lane_name(component))
        .map_err(tx)?;
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
    let opened = ComponentLane {
        component,
        lane,
        table,
        staging,
        quarantine,
        ancestors,
        ancestor_records,
        ancestor_staging,
    };
    require_exact_ancestor_scaffold(&opened)?;
    lease.revalidate().map_err(tx)?;
    Ok(opened)
}

fn component_settled_outcome(
    lease: ManagedRootPublicationLease,
    outcome: ComponentOutcomeRecord,
) -> ComponentSettledOutcome {
    match outcome.terminal {
        ComponentTerminalOutcome::Committed => ComponentSettledOutcome::Committed(lease),
        ComponentTerminalOutcome::RolledBack => ComponentSettledOutcome::RolledBack {
            lease,
            effect: outcome.effect,
        },
    }
}

fn cleanup_component_settlement(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
    settlement_guard: &ManagedFileGuard,
    fault: ComponentSettlementFault,
) -> Result<(), ComponentTransactionError> {
    validate_settlement_marker_shape(lane, lease, settlement, settlement_guard)?;

    let ancestor_plan = admit_settlement_ancestors(lane, lease, settlement)?;
    let row_plan = admit_settlement_rows(lane, lease, settlement)?;
    for shard_index in (0..ancestor_plan.record_count).rev() {
        cleanup_one_settlement_ancestor_shard(
            lane,
            lease,
            settlement,
            &ancestor_plan.created_ancestors,
            shard_index,
            fault,
        )?;
    }
    require_empty_ancestor_scaffold(lane)?;

    for shard_index in (0..row_plan.table_count).rev() {
        cleanup_one_settlement_row_shard(lane, lease, settlement, shard_index, fault)?;
    }

    validate_settled_data_scaffold(lane, lease, settlement)?;
    validate_settlement_marker_shape(lane, lease, settlement, settlement_guard)?;
    if let Some(guard) = exact_encoded_marker(
        &lane.lane,
        COMPONENT_OUTCOME_FILE,
        &encode_component_outcome(&settlement.outcome).map_err(tx)?,
        COMPONENT_OUTCOME_BYTES as u64,
    )? {
        lane.lane
            .remove_guarded_file(COMPONENT_OUTCOME_FILE, &guard)
            .map_err(tx)?;
        sync_component_lane_roots(lane, lease)?;
        if fault == ComponentSettlementFault::AfterOutcomeRemoval {
            return Err(ComponentTransactionError);
        }
    }
    validate_settlement_marker_shape(lane, lease, settlement, settlement_guard)?;
    if let Some(guard) = exact_encoded_marker(
        &lane.lane,
        COMPONENT_INTENT_FILE,
        &settlement.encoded_intent,
        MAX_COMPONENT_INTENT_BYTES as u64,
    )? {
        lane.lane
            .remove_guarded_file(COMPONENT_INTENT_FILE, &guard)
            .map_err(tx)?;
        sync_component_lane_roots(lane, lease)?;
        if fault == ComponentSettlementFault::AfterIntentRemoval {
            return Err(ComponentTransactionError);
        }
    }
    validate_settlement_marker_shape(lane, lease, settlement, settlement_guard)?;
    if !lane
        .lane
        .file_guard_matches(COMPONENT_SETTLEMENT_FILE, settlement_guard)
        .map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    lane.lane
        .remove_guarded_file(COMPONENT_SETTLEMENT_FILE, settlement_guard)
        .map_err(tx)?;
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterSettlementRemoval {
        return Err(ComponentTransactionError);
    }
    validate_marker_free_settlement_shape(lane, lease, &settlement.outcome)
}

fn validate_settlement_marker_shape(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
    settlement_guard: &ManagedFileGuard,
) -> Result<(), ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    if settlement.intent.component != lane.component
        || settlement.intent.root_binding_sha256
            != component_root_binding_sha256(lease.root()).map_err(tx)?
        || settlement
            .outcome
            .binds_intent(&settlement.intent, &settlement.encoded_intent)
            .is_err()
        || !lane
            .lane
            .file_guard_matches(COMPONENT_SETTLEMENT_FILE, settlement_guard)
            .map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    let names = exact_entry_names(&lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)?;
    let intent_present = names.contains(COMPONENT_INTENT_FILE);
    let outcome_present = names.contains(COMPONENT_OUTCOME_FILE);
    if outcome_present && !intent_present {
        return Err(ComponentTransactionError);
    }
    let mut expected = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_SETTLEMENT_FILE.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    if intent_present {
        expected.insert(COMPONENT_INTENT_FILE.to_string());
    }
    if outcome_present {
        expected.insert(COMPONENT_OUTCOME_FILE.to_string());
    }
    if names != expected {
        return Err(ComponentTransactionError);
    }
    if intent_present
        && exact_encoded_marker(
            &lane.lane,
            COMPONENT_INTENT_FILE,
            &settlement.encoded_intent,
            MAX_COMPONENT_INTENT_BYTES as u64,
        )?
        .is_none()
    {
        return Err(ComponentTransactionError);
    }
    if outcome_present
        && exact_encoded_marker(
            &lane.lane,
            COMPONENT_OUTCOME_FILE,
            &encode_component_outcome(&settlement.outcome).map_err(tx)?,
            COMPONENT_OUTCOME_BYTES as u64,
        )?
        .is_none()
    {
        return Err(ComponentTransactionError);
    }
    require_exact_ancestor_scaffold(lane)?;
    lease.revalidate().map_err(tx)
}

fn exact_encoded_marker(
    directory: &ManagedDir,
    name: &str,
    expected: &[u8],
    maximum: u64,
) -> Result<Option<ManagedFileGuard>, ComponentTransactionError> {
    let Some(guard) = directory.inspect_regular_file(name).map_err(tx)? else {
        return Ok(None);
    };
    if guard.size() != expected.len() as u64
        || directory
            .read_guarded_file_bounded(name, &guard, maximum)
            .map_err(tx)?
            != expected
    {
        return Err(ComponentTransactionError);
    }
    Ok(Some(guard))
}

fn require_exact_ancestor_scaffold(lane: &ComponentLane) -> Result<(), ComponentTransactionError> {
    if exact_entry_names(&lane.ancestors, 3).map_err(tx)?
        != BTreeSet::from([
            COMPONENT_ANCESTOR_RECORDS_DIRECTORY.to_string(),
            COMPONENT_ANCESTOR_STAGING_DIRECTORY.to_string(),
        ])
    {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn require_empty_ancestor_scaffold(lane: &ComponentLane) -> Result<(), ComponentTransactionError> {
    require_exact_ancestor_scaffold(lane)?;
    if !exact_entry_names(&lane.ancestor_records, 1)
        .map_err(tx)?
        .is_empty()
        || !exact_entry_names(&lane.ancestor_staging, 1)
            .map_err(tx)?
            .is_empty()
    {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn sync_component_lane_roots(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
) -> Result<(), ComponentTransactionError> {
    lane.lane.sync().map_err(tx)?;
    lease.publication_directory().sync().map_err(tx)?;
    lease.root().sync().map_err(tx)?;
    lease.revalidate().map_err(tx)
}

fn admit_settlement_ancestors(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
) -> Result<SettlementAncestorPlan, ComponentTransactionError> {
    let record_names =
        exact_entry_names(&lane.ancestor_records, MAX_COMPONENT_ANCESTOR_SHARDS + 1).map_err(tx)?;
    let record_count = exact_prefix_len(
        &record_names,
        MAX_COMPONENT_ANCESTOR_SHARDS,
        component_ancestor_record_file_name,
    )?;
    let mut bucket_names =
        exact_entry_names(&lane.ancestor_staging, MAX_COMPONENT_ANCESTOR_SHARDS + 2).map_err(tx)?;
    let parked_bucket = admit_empty_recovery_park(
        &lane.ancestor_staging,
        &mut bucket_names,
        COMPONENT_BUCKET_PARK_A,
        COMPONENT_BUCKET_PARK_B,
    )?;
    let bucket_count = exact_prefix_len(
        &bucket_names,
        MAX_COMPONENT_ANCESTOR_SHARDS,
        component_ancestor_bucket_name,
    )?;
    if bucket_count > record_count
        || bucket_count.saturating_add(1) < record_count
        || parked_bucket.is_some() && bucket_count.saturating_add(1) != record_count
    {
        return Err(ComponentTransactionError);
    }
    if record_count == 0 {
        if bucket_count != 0 || parked_bucket.is_some() {
            return Err(ComponentTransactionError);
        }
        return Ok(SettlementAncestorPlan {
            record_count: 0,
            created_ancestors: Vec::new(),
        });
    }

    let summary = replay_complete_settlement_table(lane, settlement)?;
    let authority = ComponentAncestorJournalAuthority::new(
        &settlement.encoded_intent,
        &summary.created_ancestors,
    )
    .map_err(tx)?;
    if record_count > authority.shard_count()
        || settlement.outcome.terminal == ComponentTerminalOutcome::Committed
            && record_count != authority.shard_count()
    {
        return Err(ComponentTransactionError);
    }
    for shard_index in 0..record_count {
        let record_name = component_ancestor_record_file_name(shard_index).map_err(tx)?;
        let record_guard = lane
            .ancestor_records
            .inspect_regular_file(&record_name)
            .map_err(tx)?
            .ok_or(ComponentTransactionError)?;
        let encoded = lane
            .ancestor_records
            .read_guarded_file_bounded(
                &record_name,
                &record_guard,
                MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES as u64,
            )
            .map_err(tx)?;
        let journal = authority.decode_shard(&encoded).map_err(tx)?;
        if journal.shard_index() != shard_index {
            return Err(ComponentTransactionError);
        }
        let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
        let bucket = (shard_index < bucket_count)
            .then(|| lane.ancestor_staging.open_child(&bucket_name).map_err(tx))
            .transpose()?;
        if let Some(bucket) = bucket.as_ref() {
            let mut names =
                exact_entry_names(bucket, COMPONENT_ANCESTOR_RECORDS_PER_SHARD + 2).map_err(tx)?;
            let parked_slot = admit_empty_recovery_park(
                bucket,
                &mut names,
                ANCESTOR_SLOT_PARK_A,
                ANCESTOR_SLOT_PARK_B,
            )?;
            let slot_count =
                exact_prefix_len(&names, journal.records().len(), component_slot_name)?;
            let is_current = shard_index + 1 == record_count;
            let expected_slot_count = match settlement.outcome.terminal {
                ComponentTerminalOutcome::Committed => 0,
                ComponentTerminalOutcome::RolledBack => journal.records().len(),
            };
            if slot_count > expected_slot_count
                || !is_current && slot_count != expected_slot_count
                || parked_slot.is_some()
                    && (!is_current || slot_count.saturating_add(1) > expected_slot_count)
            {
                return Err(ComponentTransactionError);
            }
            if let Some(parked) = parked_slot.as_ref() {
                let record = journal
                    .records()
                    .get(slot_count)
                    .ok_or(ComponentTransactionError)?;
                if !record.matches_identity(parked.directory.identity().map_err(tx)?) {
                    return Err(ComponentTransactionError);
                }
            }
            for (row_in_shard, record) in journal.records().iter().take(slot_count).enumerate() {
                let name = component_slot_name(row_in_shard).map_err(tx)?;
                let slot = bucket.open_child(&name).map_err(tx)?;
                if !record.matches_identity(slot.identity().map_err(tx)?)
                    || !exact_entry_names(&slot, 1).map_err(tx)?.is_empty()
                {
                    return Err(ComponentTransactionError);
                }
            }
        } else if shard_index + 1 != record_count {
            return Err(ComponentTransactionError);
        }

        for record in journal.records() {
            let canonical =
                open_settlement_canonical_ancestor(lease, lane.component, record.target())?;
            match settlement.outcome.terminal {
                ComponentTerminalOutcome::Committed => {
                    let canonical = canonical.ok_or(ComponentTransactionError)?;
                    if !record.matches_identity(canonical.identity().map_err(tx)?) {
                        return Err(ComponentTransactionError);
                    }
                }
                ComponentTerminalOutcome::RolledBack => {
                    if canonical.is_some() {
                        return Err(ComponentTransactionError);
                    }
                }
            }
        }
    }
    lease.revalidate().map_err(tx)?;
    Ok(SettlementAncestorPlan {
        record_count,
        created_ancestors: summary.created_ancestors,
    })
}

fn replay_complete_settlement_table(
    lane: &ComponentLane,
    settlement: &ComponentSettlementRecord,
) -> Result<ComponentTableSummary, ComponentTransactionError> {
    let expected = (0..settlement.intent.shards.len())
        .map(component_table_file_name)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(tx)?;
    if exact_entry_names(&lane.table, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)? != expected {
        return Err(ComponentTransactionError);
    }
    let mut parser = ComponentTableParser::new(settlement.intent.clone()).map_err(tx)?;
    for shard_index in 0..settlement.intent.shards.len() {
        let (shard, _) = read_settlement_table_shard(lane, &settlement.intent, shard_index)?;
        parser
            .parse_next(
                &crate::managed_component_table::encode_component_table_shard(&shard)
                    .map_err(tx)?,
            )
            .map_err(tx)?;
    }
    parser.finish().map_err(tx)
}

fn cleanup_one_settlement_ancestor_shard(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
    created_ancestors: &[ComponentCreatedAncestor],
    shard_index: usize,
    fault: ComponentSettlementFault,
) -> Result<(), ComponentTransactionError> {
    let authority =
        ComponentAncestorJournalAuthority::new(&settlement.encoded_intent, created_ancestors)
            .map_err(tx)?;
    let record_names =
        exact_entry_names(&lane.ancestor_records, MAX_COMPONENT_ANCESTOR_SHARDS + 1).map_err(tx)?;
    if exact_prefix_len(
        &record_names,
        authority.shard_count(),
        component_ancestor_record_file_name,
    )? != shard_index + 1
    {
        return Err(ComponentTransactionError);
    }
    let record_name = component_ancestor_record_file_name(shard_index).map_err(tx)?;
    let record_guard = lane
        .ancestor_records
        .inspect_regular_file(&record_name)
        .map_err(tx)?
        .ok_or(ComponentTransactionError)?;
    let encoded = lane
        .ancestor_records
        .read_guarded_file_bounded(
            &record_name,
            &record_guard,
            MAX_COMPONENT_ANCESTOR_JOURNAL_SHARD_BYTES as u64,
        )
        .map_err(tx)?;
    let journal = authority.decode_shard(&encoded).map_err(tx)?;
    if journal.shard_index() != shard_index {
        return Err(ComponentTransactionError);
    }

    let mut bucket_names =
        exact_entry_names(&lane.ancestor_staging, MAX_COMPONENT_ANCESTOR_SHARDS + 2).map_err(tx)?;
    let parked_bucket = admit_empty_recovery_park(
        &lane.ancestor_staging,
        &mut bucket_names,
        COMPONENT_BUCKET_PARK_A,
        COMPONENT_BUCKET_PARK_B,
    )?;
    let bucket_count = exact_prefix_len(
        &bucket_names,
        authority.shard_count(),
        component_ancestor_bucket_name,
    )?;
    if bucket_count > shard_index + 1
        || bucket_count < shard_index
        || parked_bucket.is_some() && bucket_count != shard_index
    {
        return Err(ComponentTransactionError);
    }
    let bucket_name = component_ancestor_bucket_name(shard_index).map_err(tx)?;
    if bucket_count == shard_index + 1 {
        let bucket = lane.ancestor_staging.open_child(&bucket_name).map_err(tx)?;
        let mut names =
            exact_entry_names(&bucket, COMPONENT_ANCESTOR_RECORDS_PER_SHARD + 2).map_err(tx)?;
        let parked_slot = admit_empty_recovery_park(
            &bucket,
            &mut names,
            ANCESTOR_SLOT_PARK_A,
            ANCESTOR_SLOT_PARK_B,
        )?;
        let expected_slots = match settlement.outcome.terminal {
            ComponentTerminalOutcome::Committed => 0,
            ComponentTerminalOutcome::RolledBack => journal.records().len(),
        };
        let slot_count = exact_prefix_len(&names, expected_slots, component_slot_name)?;
        if let Some(parked) = parked_slot.as_ref() {
            let record = journal
                .records()
                .get(slot_count)
                .ok_or(ComponentTransactionError)?;
            if !record.matches_identity(parked.directory.identity().map_err(tx)?) {
                return Err(ComponentTransactionError);
            }
        }
        for (row_in_shard, record) in journal.records().iter().take(slot_count).enumerate() {
            let name = component_slot_name(row_in_shard).map_err(tx)?;
            let slot = bucket.open_child(&name).map_err(tx)?;
            if !record.matches_identity(slot.identity().map_err(tx)?)
                || !exact_entry_names(&slot, 1).map_err(tx)?.is_empty()
            {
                return Err(ComponentTransactionError);
            }
        }
        for record in journal.records() {
            validate_settlement_ancestor_terminal(
                lease,
                lane.component,
                record,
                settlement.outcome.terminal,
            )?;
        }
        if let Some(parked) = parked_slot {
            finish_empty_recovery_park(&bucket, parked)?;
        }
        for row_in_shard in (0..slot_count).rev() {
            let name = component_slot_name(row_in_shard).map_err(tx)?;
            let slot = bucket.open_child(&name).map_err(tx)?;
            if !journal.records()[row_in_shard].matches_identity(slot.identity().map_err(tx)?)
                || bucket
                    .remove_empty_child_guarded(&name, ANCESTOR_SLOT_PARK_A, slot)
                    .map_err(tx)?
                    != ManagedEmptyChildRemoval::Removed
            {
                return Err(ComponentTransactionError);
            }
            bucket.sync().map_err(tx)?;
        }
        if lane
            .ancestor_staging
            .remove_empty_child_guarded(
                &bucket_name,
                if shard_index % 2 == 0 {
                    COMPONENT_BUCKET_PARK_A
                } else {
                    COMPONENT_BUCKET_PARK_B
                },
                bucket,
            )
            .map_err(tx)?
            != ManagedEmptyChildRemoval::Removed
        {
            return Err(ComponentTransactionError);
        }
        lane.ancestor_staging.sync().map_err(tx)?;
    } else {
        for record in journal.records() {
            validate_settlement_ancestor_terminal(
                lease,
                lane.component,
                record,
                settlement.outcome.terminal,
            )?;
        }
    }
    if let Some(parked) = parked_bucket {
        finish_empty_recovery_park(&lane.ancestor_staging, parked)?;
    }
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterAncestorBucket {
        return Err(ComponentTransactionError);
    }
    lane.ancestor_records
        .remove_guarded_file(&record_name, &record_guard)
        .map_err(tx)?;
    lane.ancestor_records.sync().map_err(tx)?;
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterAncestorRecord {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn validate_settlement_ancestor_terminal(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    record: &ComponentAncestorJournalRecord,
    terminal: ComponentTerminalOutcome,
) -> Result<(), ComponentTransactionError> {
    let canonical = open_settlement_canonical_ancestor(lease, component, record.target())?;
    match terminal {
        ComponentTerminalOutcome::Committed => {
            let canonical = canonical.ok_or(ComponentTransactionError)?;
            if !record.matches_identity(canonical.identity().map_err(tx)?) {
                return Err(ComponentTransactionError);
            }
        }
        ComponentTerminalOutcome::RolledBack if canonical.is_none() => {}
        ComponentTerminalOutcome::RolledBack => return Err(ComponentTransactionError),
    }
    Ok(())
}

fn open_settlement_canonical_ancestor(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    target: &ComponentCreatedAncestor,
) -> Result<Option<ManagedDir>, ComponentTransactionError> {
    lease.revalidate().map_err(tx)?;
    let root = lease.root();
    let component_root = component_lane_name(component);
    let result = (|| match target {
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
    })()?;
    lease.revalidate().map_err(tx)?;
    Ok(result)
}

fn admit_settlement_rows(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
) -> Result<SettlementRowsPlan, ComponentTransactionError> {
    let table_names = exact_entry_names(&lane.table, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)?;
    let table_count = exact_prefix_len(
        &table_names,
        settlement.intent.shards.len(),
        component_table_file_name,
    )?;
    let (staging_count, staging_park) =
        admit_settlement_bucket_parent(&lane.staging, settlement.intent.shards.len())?;
    let (quarantine_count, quarantine_park) =
        admit_settlement_bucket_parent(&lane.quarantine, settlement.intent.shards.len())?;
    if staging_count > table_count
        || quarantine_count > table_count
        || staging_count.saturating_add(1) < table_count
        || quarantine_count.saturating_add(1) < table_count
        || staging_count > quarantine_count
        || staging_park.is_some() && staging_count.saturating_add(1) != table_count
        || quarantine_park.is_some() && quarantine_count.saturating_add(1) != table_count
        || staging_park.is_some() && quarantine_park.is_some()
        || quarantine_count < table_count && staging_count == table_count
    {
        return Err(ComponentTransactionError);
    }
    if table_count == 0 {
        if staging_count != 0
            || quarantine_count != 0
            || staging_park.is_some()
            || quarantine_park.is_some()
        {
            return Err(ComponentTransactionError);
        }
        return Ok(SettlementRowsPlan { table_count: 0 });
    }

    for shard_index in 0..table_count {
        let (shard, table_guard) =
            read_settlement_table_shard(lane, &settlement.intent, shard_index)?;
        let table_name = component_table_file_name(shard_index).map_err(tx)?;
        let bucket_name = component_bucket_name(shard_index).map_err(tx)?;
        let is_current = shard_index + 1 == table_count;
        let staging_directory = (shard_index < staging_count)
            .then(|| lane.staging.open_child(&bucket_name).map_err(tx))
            .transpose()?;
        let quarantine_directory = (shard_index < quarantine_count)
            .then(|| lane.quarantine.open_child(&bucket_name).map_err(tx))
            .transpose()?;

        let mut expected_staging = Vec::new();
        let mut expected_quarantine = Vec::new();
        expected_staging
            .try_reserve_exact(shard.rows.len())
            .map_err(tx)?;
        expected_quarantine
            .try_reserve_exact(shard.rows.len())
            .map_err(tx)?;
        for (row_in_shard, row) in shard.rows.iter().enumerate() {
            validate_settlement_canonical_row(
                lease,
                lane.component,
                row,
                settlement.outcome.terminal,
            )?;
            let slot = component_slot_name(row_in_shard).map_err(tx)?;
            match settlement.outcome.terminal {
                ComponentTerminalOutcome::Committed => {
                    if let Some(prior) = row.prior.as_ref().filter(|_| !row.prior_is_final()) {
                        expected_quarantine.push((slot, prior.size, prior.sha1));
                    }
                }
                ComponentTerminalOutcome::RolledBack => {
                    if !row.prior_is_final() {
                        expected_staging.push((slot, row.final_size, row.final_sha1));
                    }
                }
            }
        }

        let staging_files = validate_settlement_residue_bucket(
            staging_directory.as_ref(),
            &expected_staging,
            is_current,
        )?;
        let quarantine_files = validate_settlement_residue_bucket(
            quarantine_directory.as_ref(),
            &expected_quarantine,
            is_current,
        )?;
        if !is_current
            && (staging_files != expected_staging.len()
                || quarantine_files != expected_quarantine.len())
        {
            return Err(ComponentTransactionError);
        }
        drop((table_name, table_guard, bucket_name));
    }
    lease.revalidate().map_err(tx)?;
    Ok(SettlementRowsPlan { table_count })
}

fn admit_settlement_bucket_parent(
    parent: &ManagedDir,
    maximum: usize,
) -> Result<(usize, Option<EmptyRecoveryPark>), ComponentTransactionError> {
    let mut names = exact_entry_names(parent, maximum + 2).map_err(tx)?;
    let parked = admit_empty_recovery_park(
        parent,
        &mut names,
        COMPONENT_BUCKET_PARK_A,
        COMPONENT_BUCKET_PARK_B,
    )?;
    let count = exact_prefix_len(&names, maximum, component_bucket_name)?;
    Ok((count, parked))
}

fn validate_settlement_residue_bucket(
    directory: Option<&ManagedDir>,
    expected: &[(String, u64, [u8; 20])],
    allow_partial: bool,
) -> Result<usize, ComponentTransactionError> {
    let Some(directory) = directory else {
        return Ok(0);
    };
    let names = exact_entry_names(directory, expected.len() + 1).map_err(tx)?;
    let prefix = expected
        .iter()
        .take_while(|(name, _, _)| names.contains(name))
        .count();
    let expected_names = expected
        .iter()
        .take(prefix)
        .map(|(name, _, _)| name.clone())
        .collect::<BTreeSet<_>>();
    if names != expected_names || !allow_partial && prefix != expected.len() {
        return Err(ComponentTransactionError);
    }
    for (name, size, sha1) in expected.iter().take(prefix) {
        let _ = exact_file(directory, name, *size, *sha1)?.ok_or(ComponentTransactionError)?;
    }
    Ok(prefix)
}

fn cleanup_one_settlement_row_shard(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
    shard_index: usize,
    fault: ComponentSettlementFault,
) -> Result<(), ComponentTransactionError> {
    let table_names = exact_entry_names(&lane.table, MAX_COMPONENT_TABLE_SHARDS + 1).map_err(tx)?;
    if exact_prefix_len(
        &table_names,
        settlement.intent.shards.len(),
        component_table_file_name,
    )? != shard_index + 1
    {
        return Err(ComponentTransactionError);
    }
    let (shard, table_guard) = read_settlement_table_shard(lane, &settlement.intent, shard_index)?;
    let table_name = component_table_file_name(shard_index).map_err(tx)?;
    let bucket_name = component_bucket_name(shard_index).map_err(tx)?;
    let mut expected_staging = Vec::new();
    let mut expected_quarantine = Vec::new();
    expected_staging
        .try_reserve_exact(shard.rows.len())
        .map_err(tx)?;
    expected_quarantine
        .try_reserve_exact(shard.rows.len())
        .map_err(tx)?;
    for (row_in_shard, row) in shard.rows.iter().enumerate() {
        validate_settlement_canonical_row(lease, lane.component, row, settlement.outcome.terminal)?;
        let slot = component_slot_name(row_in_shard).map_err(tx)?;
        match settlement.outcome.terminal {
            ComponentTerminalOutcome::Committed => {
                if let Some(prior) = row.prior.as_ref().filter(|_| !row.prior_is_final()) {
                    expected_quarantine.push((slot, prior.size, prior.sha1));
                }
            }
            ComponentTerminalOutcome::RolledBack => {
                if !row.prior_is_final() {
                    expected_staging.push((slot, row.final_size, row.final_sha1));
                }
            }
        }
    }
    cleanup_one_settlement_residue_bucket(
        &lane.staging,
        settlement.intent.shards.len(),
        shard_index,
        &bucket_name,
        &expected_staging,
    )?;
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterStagingBucket {
        return Err(ComponentTransactionError);
    }
    cleanup_one_settlement_residue_bucket(
        &lane.quarantine,
        settlement.intent.shards.len(),
        shard_index,
        &bucket_name,
        &expected_quarantine,
    )?;
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterQuarantineBucket {
        return Err(ComponentTransactionError);
    }
    lane.table
        .remove_guarded_file(&table_name, &table_guard)
        .map_err(tx)?;
    lane.table.sync().map_err(tx)?;
    sync_component_lane_roots(lane, lease)?;
    if fault == ComponentSettlementFault::AfterTableShard {
        return Err(ComponentTransactionError);
    }
    Ok(())
}

fn cleanup_one_settlement_residue_bucket(
    parent: &ManagedDir,
    maximum_shards: usize,
    shard_index: usize,
    bucket_name: &str,
    expected: &[(String, u64, [u8; 20])],
) -> Result<(), ComponentTransactionError> {
    let (bucket_count, parked) = admit_settlement_bucket_parent(parent, maximum_shards)?;
    if bucket_count > shard_index + 1
        || bucket_count < shard_index
        || parked.is_some() && bucket_count != shard_index
    {
        return Err(ComponentTransactionError);
    }
    if bucket_count == shard_index + 1 {
        let bucket = parent.open_child(bucket_name).map_err(tx)?;
        let prefix = validate_settlement_residue_bucket(Some(&bucket), expected, true)?;
        for (name, size, sha1) in expected.iter().take(prefix).rev() {
            let guard =
                exact_file(&bucket, name, *size, *sha1)?.ok_or(ComponentTransactionError)?;
            bucket.remove_guarded_file(name, &guard).map_err(tx)?;
            bucket.sync().map_err(tx)?;
        }
        if parent
            .remove_empty_child_guarded(
                bucket_name,
                if shard_index % 2 == 0 {
                    COMPONENT_BUCKET_PARK_A
                } else {
                    COMPONENT_BUCKET_PARK_B
                },
                bucket,
            )
            .map_err(tx)?
            != ManagedEmptyChildRemoval::Removed
        {
            return Err(ComponentTransactionError);
        }
        parent.sync().map_err(tx)?;
    }
    if let Some(parked) = parked {
        finish_empty_recovery_park(parent, parked)?;
    }
    Ok(())
}

fn validate_settlement_canonical_row(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    row: &ComponentTableRow,
    terminal: ComponentTerminalOutcome,
) -> Result<(), ComponentTransactionError> {
    let plan = plan_component_canonical_path(lease.root(), component, &row.path).map_err(tx)?;
    let observed = match plan.observe().map_err(tx)? {
        ComponentCanonicalObservation::Absent => None,
        ComponentCanonicalObservation::Regular(file) => Some(file),
    };
    let exact = match terminal {
        ComponentTerminalOutcome::Committed => observed
            .as_ref()
            .is_some_and(|file| file.size == row.final_size && file.sha1 == row.final_sha1),
        ComponentTerminalOutcome::RolledBack => match (&row.prior, observed.as_ref()) {
            (None, None) => true,
            (Some(prior), Some(file)) => file.size == prior.size && file.sha1 == prior.sha1,
            _ => false,
        },
    };
    if !exact {
        return Err(ComponentTransactionError);
    }
    lease.revalidate().map_err(tx)
}

fn read_settlement_table_shard(
    lane: &ComponentLane,
    manifest: &ComponentIntentManifest,
    shard_index: usize,
) -> Result<(ComponentTableShard, ManagedFileGuard), ComponentTransactionError> {
    let descriptor = manifest
        .shards
        .get(shard_index)
        .ok_or(ComponentTransactionError)?;
    let name = component_table_file_name(shard_index).map_err(tx)?;
    let guard = lane
        .table
        .inspect_regular_file(&name)
        .map_err(tx)?
        .ok_or(ComponentTransactionError)?;
    if guard.size() != u64::from(descriptor.byte_len)
        || guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64
    {
        return Err(ComponentTransactionError);
    }
    let bytes = lane
        .table
        .read_guarded_file_bounded(&name, &guard, MAX_COMPONENT_TABLE_SHARD_BYTES as u64)
        .map_err(tx)?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != descriptor.sha256 {
        return Err(ComponentTransactionError);
    }
    let shard = decode_component_table_shard(&bytes).map_err(tx)?;
    if usize::try_from(shard.shard_index).map_err(tx)? != shard_index
        || shard.shard_index != descriptor.shard_index
        || shard.first_row != descriptor.first_row
        || usize::try_from(descriptor.row_count).map_err(tx)? != shard.rows.len()
        || shard.total_rows != manifest.total_rows
        || shard.component != manifest.component
        || shard.transaction_nonce != manifest.transaction_nonce
        || shard.root_binding_sha256 != manifest.root_binding_sha256
    {
        return Err(ComponentTransactionError);
    }
    Ok((shard, guard))
}

fn validate_settled_data_scaffold(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    settlement: &ComponentSettlementRecord,
) -> Result<(), ComponentTransactionError> {
    require_empty_ancestor_scaffold(lane)?;
    if !exact_entry_names(&lane.table, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&lane.staging, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&lane.quarantine, 1)
            .map_err(tx)?
            .is_empty()
        || settlement.intent.root_binding_sha256
            != component_root_binding_sha256(lease.root()).map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    lease.revalidate().map_err(tx)
}

fn validate_marker_free_settlement_shape(
    lane: &ComponentLane,
    lease: &ManagedRootPublicationLease,
    outcome: &ComponentOutcomeRecord,
) -> Result<(), ComponentTransactionError> {
    let expected = BTreeSet::from([
        COMPONENT_ANCESTORS_DIRECTORY.to_string(),
        COMPONENT_QUARANTINE_DIRECTORY.to_string(),
        COMPONENT_STAGING_DIRECTORY.to_string(),
        COMPONENT_TABLE_DIRECTORY.to_string(),
    ]);
    if exact_entry_names(&lane.lane, MAX_COMPONENT_LANE_ENTRIES + 1).map_err(tx)? != expected
        || outcome.component != lane.component
        || outcome.root_binding_sha256 != component_root_binding_sha256(lease.root()).map_err(tx)?
    {
        return Err(ComponentTransactionError);
    }
    require_empty_ancestor_scaffold(lane)?;
    if !exact_entry_names(&lane.table, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&lane.staging, 1).map_err(tx)?.is_empty()
        || !exact_entry_names(&lane.quarantine, 1)
            .map_err(tx)?
            .is_empty()
    {
        return Err(ComponentTransactionError);
    }
    lease.revalidate().map_err(tx)
}

fn finish_settlement_disposition(
    shared: &Mutex<Option<ComponentSettlementAuthority>>,
    disposition: SettlementDisposition,
) -> ComponentSettlementResult {
    let authority = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("component settlement owner must retain its authority");
    if disposition == SettlementDisposition::Retry {
        return ComponentSettlementResult::Retry(ComponentSettlementRetry { authority });
    }
    let ComponentSettlementAuthority {
        context,
        outcome: Some(outcome),
        ..
    } = authority
    else {
        panic!("settled component must retain its terminal outcome")
    };
    ComponentSettlementResult::Settled(component_settled_outcome(context.lease, outcome))
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
        BlockingDisposition::NoTransaction
        | BlockingDisposition::RetryIntent
        | BlockingDisposition::Settled(_) => {
            unreachable!("live execution cannot return a recovery admission disposition")
        }
        BlockingDisposition::Committed(outcome_guard) => {
            ComponentExecutionResult::Committed(ComponentTransactionReceipt {
                context,
                outcome_guard,
                terminal: ComponentTerminalOutcome::Committed,
            })
        }
        BlockingDisposition::RolledBack(outcome_guard) => {
            ComponentExecutionResult::RolledBack(ComponentTransactionReceipt {
                context,
                outcome_guard,
                terminal: ComponentTerminalOutcome::RolledBack,
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
            BlockingDisposition::Settled(outcome),
            ComponentRecoveryAuthority::Restart { lease, .. },
        ) => RecoveryOwnerResult::Settled(component_settled_outcome(lease, outcome)),
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
            ComponentTransactionReceipt {
                context,
                outcome_guard,
                terminal: ComponentTerminalOutcome::Committed,
            },
        )),
        (
            BlockingDisposition::RolledBack(outcome_guard),
            ComponentRecoveryAuthority::Published { context, .. },
        ) => RecoveryOwnerResult::Transaction(ComponentExecutionResult::RolledBack(
            ComponentTransactionReceipt {
                context,
                outcome_guard,
                terminal: ComponentTerminalOutcome::RolledBack,
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

#[cfg(all(test, target_os = "linux"))]
mod settlement_resource_tests {
    use super::*;
    use std::fs;

    fn open_fds_beneath(root: &std::path::Path) -> usize {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(root))
            .count()
    }

    #[test]
    fn maximum_settlement_shard_frontier_retains_constant_handles() {
        let temporary = tempfile::tempdir().unwrap();
        for shard_index in 0..MAX_COMPONENT_TABLE_SHARDS {
            fs::create_dir(
                temporary
                    .path()
                    .join(component_bucket_name(shard_index).unwrap()),
            )
            .unwrap();
        }
        let parent = ManagedDir::open_root(temporary.path()).unwrap();
        let before = open_fds_beneath(temporary.path());

        let (count, parked) = admit_settlement_bucket_parent(&parent, MAX_COMPONENT_TABLE_SHARDS)
            .unwrap_or_else(|_| panic!("maximum settlement bucket frontier must be admitted"));

        assert_eq!(count, MAX_COMPONENT_TABLE_SHARDS);
        assert!(parked.is_none());
        assert!(open_fds_beneath(temporary.path()) <= before + 1);
    }
}
