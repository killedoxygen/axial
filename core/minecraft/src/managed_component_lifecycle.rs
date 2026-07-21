use crate::portable_path::PortableRelativePath;
#[cfg(test)]
use crate::known_good::PendingKnownGoodInstallAuthority;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRoot, ManagedComponentProjection,
    ManagedKnownGoodComponent,
};
use crate::loaders::types::LoaderError;
use crate::managed_component_effects::{
    ComponentCanonicalObservation, ComponentEffectsError, ComponentExecutionResult,
    ComponentIntentCandidate, ComponentIntentPublicationRecovery, ComponentIntentPublishFailure,
    ComponentLane, ComponentPreparedCanonicalAuthority, ComponentPriorRecoveryRetryResult,
    ComponentRecoveryRetryResult, ComponentSettledOutcome, ComponentSettlementResult,
    ComponentStartupRecoveryResult, component_root_binding_sha256, component_slot_name,
    execute_component_intent, plan_component_canonical_path, recover_component_intent_publication,
    recover_component_transaction, retry_component_recovery, retry_component_settlement,
    retry_prior_component_recovery, settle_component_transaction,
};
#[cfg(test)]
use crate::managed_component_effects::{
    ComponentExecutionFault, ComponentIntentPublishFault, ComponentSettlementFault,
    execute_component_intent_with_fault, settle_component_transaction_with_fault,
};
use crate::managed_component_spool::{ComponentTableSpool, ComponentTableSpoolError};
use crate::managed_component_table::{
    COMPONENT_TABLE_ROWS_PER_SHARD, ComponentIntentManifest, ComponentPriorFile,
    ComponentTableBuilder, ComponentTableError, ComponentTableRow, ComponentTableSummary,
    ManagedComponentArtifactKind, ManagedComponentKind,
};
use crate::managed_fs::{ManagedDir, ManagedFileIdentity};
use crate::managed_publication::{
    ManagedPublicationError, ManagedPublicationLifetimeGuard, ManagedRootPublicationLease,
    run_publication_blocking,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

const COMPONENT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(25);
const COMPONENT_RETRY_MAXIMUM_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ComponentPublicationSourceIdentity {
    relative_path: PortableRelativePath,
    kind: ManagedComponentArtifactKind,
    size: u64,
    sha1: [u8; 20],
}

pub(crate) struct StagedComponentPublicationSource {
    source: ComponentPublicationSourceIdentity,
    file: ManagedFileIdentity,
}

pub(crate) trait RetainedComponentPublicationSource: Send + Sized {
    fn relative_path(&self) -> &PortableRelativePath;
    fn kind(&self) -> ManagedComponentArtifactKind;
    fn observed_size(&self) -> u64;
    fn observed_sha1(&self) -> [u8; 20];

    fn stage_create_new(
        self,
        staging_bucket: &ManagedDir,
        slot: &str,
        lifetime_guard: ManagedPublicationLifetimeGuard,
    ) -> impl Future<Output = Result<StagedComponentPublicationSource, LoaderError>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareComponentIntentError {
    #[error("authenticated component projection is invalid")]
    Projection,
    #[error("retained component sources do not match the authenticated projection")]
    SourceSet,
    #[error("managed component projection changed before no-effect settlement")]
    CanonicalChanged,
    #[error("managed component table summaries disagree")]
    TableSummary,
    #[error(transparent)]
    Effects(#[from] ComponentEffectsError),
    #[error(transparent)]
    Table(#[from] ComponentTableError),
    #[error(transparent)]
    Spool(#[from] ComponentTableSpoolError),
    #[error(transparent)]
    Filesystem(#[from] LoaderError),
    #[error(transparent)]
    Publication(#[from] ManagedPublicationError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ComponentLifecycleError {
    #[error(transparent)]
    Prepare(#[from] PrepareComponentIntentError),
    #[error("managed component intent failed before canonical effects")]
    BeforeEffects(#[source] ComponentEffectsError),
}

impl ComponentPublicationSourceIdentity {
    pub(crate) fn new(
        relative_path: PortableRelativePath,
        kind: ManagedComponentArtifactKind,
        size: u64,
        sha1: [u8; 20],
    ) -> Self {
        Self {
            relative_path,
            kind,
            size,
            sha1,
        }
    }

    fn matches(
        &self,
        relative_path: &PortableRelativePath,
        kind: ManagedComponentArtifactKind,
        size: u64,
        sha1: [u8; 20],
    ) -> bool {
        self.relative_path == *relative_path
            && self.kind == kind
            && self.size == size
            && self.sha1 == sha1
    }
}

impl StagedComponentPublicationSource {
    pub(crate) fn new(
        source: ComponentPublicationSourceIdentity,
        file: ManagedFileIdentity,
    ) -> Self {
        Self { source, file }
    }

    pub(crate) fn matches(
        &self,
        relative_path: &PortableRelativePath,
        kind: ManagedComponentArtifactKind,
        size: u64,
        sha1: [u8; 20],
    ) -> bool {
        self.source.matches(relative_path, kind, size, sha1)
    }

    fn file_identity(&self) -> ManagedFileIdentity {
        self.file
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ComponentProjectionRow {
    inventory_ordinal: u32,
    path: PortableRelativePath,
    kind: ManagedComponentArtifactKind,
    size: u64,
    sha1: [u8; 20],
}

pub(crate) struct ManagedComponentCommittedReceipt {
    component: ManagedComponentKind,
    projection: Vec<ComponentProjectionRow>,
    lease: ManagedRootPublicationLease,
}

pub(crate) struct ManagedComponentRolledBackReceipt {
    component: ManagedComponentKind,
    projection: Vec<ComponentProjectionRow>,
    effect: crate::managed_component_publication::ComponentRollbackEffect,
    lease: ManagedRootPublicationLease,
}

pub(crate) enum ManagedComponentLifecycleOutcome {
    Committed(ManagedComponentCommittedReceipt),
    RolledBack(ManagedComponentRolledBackReceipt),
}

impl ManagedComponentCommittedReceipt {
    pub(crate) fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        projection_rows(self.component, projection).is_ok_and(|rows| rows == self.projection)
    }

    pub(crate) async fn revalidate(&self) -> bool {
        revalidate_component_projection(&self.lease, self.component, &self.projection).await
    }

    pub(crate) async fn matches_root(&self, expected: &Path) -> bool {
        receipt_matches_root(&self.lease, expected).await
    }

    pub(crate) fn into_lease(self) -> ManagedRootPublicationLease {
        self.lease
    }
}

impl ManagedComponentRolledBackReceipt {
    pub(crate) fn rollback_effect(
        &self,
    ) -> crate::managed_component_publication::ComponentRollbackEffect {
        self.effect
    }

    pub(crate) fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        projection_rows(self.component, projection).is_ok_and(|rows| rows == self.projection)
    }

    pub(crate) async fn matches_root(&self, expected: &Path) -> bool {
        receipt_matches_root(&self.lease, expected).await
    }
}

async fn receipt_matches_root(lease: &ManagedRootPublicationLease, expected: &Path) -> bool {
    if lease.revalidate().is_err() {
        return false;
    }
    let expected = expected.to_path_buf();
    let root = lease.root().clone();
    let matches = run_publication_blocking(move || {
        root.revalidate()?;
        Ok::<_, LoaderError>(ManagedDir::open_root(&expected)?.identity()? == root.identity()?)
    })
    .await;
    matches!(matches, Ok(Ok(true))) && lease.revalidate().is_ok()
}

struct SparseSourceCounts {
    required: usize,
    supplied_exact: usize,
}

struct PlannedComponentProjection {
    table_rows: Vec<ComponentTableRow>,
    authority: Vec<ComponentPreparedCanonicalAuthority>,
    all_exact: bool,
}

struct ComponentRetryBackoff {
    delay: Duration,
}

enum ComponentProgress {
    Execution(ComponentExecutionResult),
    Recovery(crate::managed_component_effects::ComponentRecoveryRequired),
    Settlement(ComponentSettlementResult),
}

#[cfg(test)]
#[derive(Default)]
struct ComponentLifecycleTestFaults {
    intent: Option<ComponentIntentPublishFault>,
    execution: Option<ComponentExecutionFault>,
    settlement: Option<ComponentSettlementFault>,
    no_effect_before_final_validation: Option<Box<dyn FnOnce() + Send>>,
}

impl ComponentRetryBackoff {
    fn new() -> Self {
        Self {
            delay: COMPONENT_RETRY_INITIAL_DELAY,
        }
    }

    async fn wait(&mut self) {
        tokio::time::sleep(self.delay).await;
        self.delay = self
            .delay
            .saturating_mul(2)
            .min(COMPONENT_RETRY_MAXIMUM_DELAY);
    }
}

#[cfg(test)]
async fn publish_managed_component_with_faults<S>(
    lease: ManagedRootPublicationLease,
    authority: &PendingKnownGoodInstallAuthority,
    component: ManagedComponentKind,
    sources: Vec<S>,
    faults: &mut ComponentLifecycleTestFaults,
) -> Result<ManagedComponentLifecycleOutcome, ComponentLifecycleError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let (known_good_component, _) = component_projection_contract(component);
    let projection = authority
        .component_projection(known_good_component)
        .map_err(|_| PrepareComponentIntentError::Projection)?;
    publish_managed_component_effect_inner(lease, projection, component, sources, Some(faults))
        .await
}

pub(crate) async fn publish_managed_component_effect<S>(
    lease: ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
    component: ManagedComponentKind,
    sources: Vec<S>,
) -> Result<ManagedComponentLifecycleOutcome, ComponentLifecycleError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    publish_managed_component_effect_inner(
        lease,
        projection,
        component,
        sources,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn publish_managed_component_effect_rolling_back_after_first_row_for_test<S>(
    lease: ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
    component: ManagedComponentKind,
    sources: Vec<S>,
) -> Result<ManagedComponentLifecycleOutcome, ComponentLifecycleError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let mut faults = ComponentLifecycleTestFaults {
        execution: Some(ComponentExecutionFault::AfterFirstRow),
        ..ComponentLifecycleTestFaults::default()
    };
    publish_managed_component_effect_inner(lease, projection, component, sources, Some(&mut faults))
        .await
}

async fn publish_managed_component_effect_inner<S>(
    lease: ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
    component: ManagedComponentKind,
    sources: Vec<S>,
    #[cfg(test)] mut faults: Option<&mut ComponentLifecycleTestFaults>,
) -> Result<ManagedComponentLifecycleOutcome, ComponentLifecycleError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let lease = settle_prior_component_transaction(lease, component).await?;
    let (lease, projection, planned, sources, source_counts) =
        plan_component_publication(lease, projection, component, sources).await?;
    if planned.all_exact {
        if source_counts.required != 0 || sources.len() != source_counts.supplied_exact {
            return Err(PrepareComponentIntentError::SourceSet.into());
        }
        drop(sources);
        #[cfg(test)]
        if let Some(hook) = faults
            .as_deref_mut()
            .and_then(|faults| faults.no_effect_before_final_validation.take())
        {
            hook();
        }
        let lease =
            revalidate_exact_component_projection(lease, component, planned.table_rows).await?;
        return Ok(ManagedComponentLifecycleOutcome::Committed(
            ManagedComponentCommittedReceipt {
                component,
                projection,
                lease,
            },
        ));
    }
    let candidate = prepare_component_intent_from_plan(
        lease,
        component,
        planned.table_rows,
        planned.authority,
        sources,
        source_counts,
    )
    .await?;
    let mut backoff = ComponentRetryBackoff::new();
    let execution = publish_current_component_intent(
        candidate,
        &mut backoff,
        #[cfg(test)]
        faults.as_deref_mut(),
    )
    .await?;
    match normalize_current_component_transaction(
        execution,
        &mut backoff,
        #[cfg(test)]
        faults,
    )
    .await?
    {
        ComponentSettledOutcome::Committed(lease) => Ok(
            ManagedComponentLifecycleOutcome::Committed(ManagedComponentCommittedReceipt {
                component,
                projection,
                lease,
            }),
        ),
        ComponentSettledOutcome::RolledBack { lease, effect } => Ok(
            ManagedComponentLifecycleOutcome::RolledBack(ManagedComponentRolledBackReceipt {
                component,
                projection,
                effect,
                lease,
            }),
        ),
    }
}

async fn settle_prior_component_transaction(
    lease: ManagedRootPublicationLease,
    component: ManagedComponentKind,
) -> Result<ManagedRootPublicationLease, ComponentLifecycleError> {
    let recovered = recover_component_transaction(lease, component).await;
    let mut backoff = ComponentRetryBackoff::new();
    let normalized = match recovered {
        ComponentStartupRecoveryResult::NoTransaction(lease) => return Ok(lease),
        ComponentStartupRecoveryResult::Settled(outcome) => outcome,
        ComponentStartupRecoveryResult::Transaction(execution) => {
            return normalize_prior_component_transaction(execution, &mut backoff).await;
        }
    };
    Ok(match normalized {
        ComponentSettledOutcome::Committed(lease)
        | ComponentSettledOutcome::RolledBack { lease, .. } => lease,
    })
}

async fn publish_current_component_intent(
    mut candidate: ComponentIntentCandidate,
    backoff: &mut ComponentRetryBackoff,
    #[cfg(test)] mut faults: Option<&mut ComponentLifecycleTestFaults>,
) -> Result<ComponentExecutionResult, ComponentLifecycleError> {
    loop {
        #[cfg(test)]
        let publication = match faults
            .as_deref_mut()
            .and_then(|faults| faults.intent.take())
        {
            Some(fault) => candidate.publish_intent_with_fault(fault),
            None => candidate.publish_intent(),
        };
        #[cfg(not(test))]
        let publication = candidate.publish_intent();
        match publication {
            Ok(published) => {
                return Ok(execute_current_component_intent(
                    published,
                    #[cfg(test)]
                    faults.as_deref_mut(),
                )
                .await);
            }
            Err(ComponentIntentPublishFailure::BeforePromotion { candidate, cause }) => {
                drop(candidate);
                return Err(ComponentLifecycleError::BeforeEffects(cause));
            }
            Err(failure @ ComponentIntentPublishFailure::PromotionAttempted { .. }) => {
                let mut recovery = failure;
                loop {
                    match recover_component_intent_publication(recovery).await {
                        Ok(ComponentIntentPublicationRecovery::Retry(retry)) => {
                            candidate = *retry;
                            backoff.wait().await;
                            break;
                        }
                        Ok(ComponentIntentPublicationRecovery::Transaction(execution)) => {
                            return Ok(execution);
                        }
                        Err(ComponentIntentPublishFailure::BeforePromotion {
                            candidate,
                            cause,
                        }) => {
                            drop(candidate);
                            return Err(ComponentLifecycleError::BeforeEffects(cause));
                        }
                        Err(next @ ComponentIntentPublishFailure::PromotionAttempted { .. }) => {
                            recovery = next;
                            backoff.wait().await;
                        }
                    }
                }
            }
        }
    }
}

async fn normalize_current_component_transaction(
    execution: ComponentExecutionResult,
    backoff: &mut ComponentRetryBackoff,
    #[cfg(test)] mut faults: Option<&mut ComponentLifecycleTestFaults>,
) -> Result<ComponentSettledOutcome, ComponentLifecycleError> {
    let mut progress = ComponentProgress::Execution(execution);
    loop {
        progress = match progress {
            ComponentProgress::Execution(ComponentExecutionResult::Committed(receipt))
            | ComponentProgress::Execution(ComponentExecutionResult::RolledBack(receipt)) => {
                ComponentProgress::Settlement(
                    settle_current_component_transaction(
                        receipt,
                        #[cfg(test)]
                        faults.as_deref_mut(),
                    )
                    .await,
                )
            }
            ComponentProgress::Execution(ComponentExecutionResult::RecoveryRequired(recovery)) => {
                ComponentProgress::Recovery(recovery)
            }
            ComponentProgress::Recovery(recovery) => {
                backoff.wait().await;
                match retry_component_recovery(recovery).await {
                    ComponentRecoveryRetryResult::Settled(outcome) => {
                        return Ok(outcome);
                    }
                    ComponentRecoveryRetryResult::RetryIntent(candidate) => {
                        ComponentProgress::Execution(
                            publish_current_component_intent(
                                *candidate,
                                backoff,
                                #[cfg(test)]
                                faults.as_deref_mut(),
                            )
                            .await?,
                        )
                    }
                    ComponentRecoveryRetryResult::Transaction(execution) => {
                        ComponentProgress::Execution(execution)
                    }
                    ComponentRecoveryRetryResult::Retry(recovery) => {
                        ComponentProgress::Recovery(recovery)
                    }
                }
            }
            ComponentProgress::Settlement(ComponentSettlementResult::Settled(outcome)) => {
                return Ok(outcome);
            }
            ComponentProgress::Settlement(ComponentSettlementResult::Retry(retry)) => {
                backoff.wait().await;
                ComponentProgress::Settlement(retry_component_settlement(retry).await)
            }
        };
    }
}

async fn normalize_prior_component_transaction(
    execution: ComponentExecutionResult,
    backoff: &mut ComponentRetryBackoff,
) -> Result<ManagedRootPublicationLease, ComponentLifecycleError> {
    let mut progress = ComponentProgress::Execution(execution);
    loop {
        progress = match progress {
            ComponentProgress::Execution(ComponentExecutionResult::Committed(receipt))
            | ComponentProgress::Execution(ComponentExecutionResult::RolledBack(receipt)) => {
                ComponentProgress::Settlement(settle_component_transaction(receipt).await)
            }
            ComponentProgress::Execution(ComponentExecutionResult::RecoveryRequired(recovery)) => {
                ComponentProgress::Recovery(recovery)
            }
            ComponentProgress::Recovery(recovery) => {
                backoff.wait().await;
                match retry_prior_component_recovery(recovery).await {
                    ComponentPriorRecoveryRetryResult::NoTransaction(lease) => return Ok(lease),
                    ComponentPriorRecoveryRetryResult::Settled(outcome) => {
                        return Ok(settled_component_lease(outcome));
                    }
                    ComponentPriorRecoveryRetryResult::RetryIntent(candidate) => {
                        ComponentProgress::Execution(
                            publish_current_component_intent(
                                *candidate,
                                backoff,
                                #[cfg(test)]
                                None,
                            )
                            .await?,
                        )
                    }
                    ComponentPriorRecoveryRetryResult::Transaction(execution) => {
                        ComponentProgress::Execution(execution)
                    }
                }
            }
            ComponentProgress::Settlement(ComponentSettlementResult::Settled(outcome)) => {
                return Ok(settled_component_lease(outcome));
            }
            ComponentProgress::Settlement(ComponentSettlementResult::Retry(retry)) => {
                backoff.wait().await;
                ComponentProgress::Settlement(retry_component_settlement(retry).await)
            }
        };
    }
}

fn settled_component_lease(outcome: ComponentSettledOutcome) -> ManagedRootPublicationLease {
    match outcome {
        ComponentSettledOutcome::Committed(lease)
        | ComponentSettledOutcome::RolledBack { lease, .. } => lease,
    }
}

async fn execute_current_component_intent(
    published: crate::managed_component_effects::ComponentIntentPublished,
    #[cfg(test)] faults: Option<&mut ComponentLifecycleTestFaults>,
) -> ComponentExecutionResult {
    #[cfg(test)]
    if let Some(fault) = faults.and_then(|faults| faults.execution.take()) {
        return execute_component_intent_with_fault(published, fault).await;
    }
    execute_component_intent(published).await
}

async fn settle_current_component_transaction(
    receipt: crate::managed_component_effects::ComponentTransactionReceipt,
    #[cfg(test)] faults: Option<&mut ComponentLifecycleTestFaults>,
) -> ComponentSettlementResult {
    #[cfg(test)]
    if let Some(fault) = faults.and_then(|faults| faults.settlement.take()) {
        return settle_component_transaction_with_fault(receipt, fault).await;
    }
    settle_component_transaction(receipt).await
}

#[cfg(test)]
async fn prepare_component_intent_projection<S>(
    lease: ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
    component: ManagedComponentKind,
    sources: Vec<S>,
) -> Result<(ComponentIntentCandidate, Vec<ComponentProjectionRow>), PrepareComponentIntentError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let (lease, projection, planned, sources, source_counts) =
        plan_component_publication(lease, projection, component, sources).await?;
    let candidate = prepare_component_intent_from_plan(
        lease,
        component,
        planned.table_rows,
        planned.authority,
        sources,
        source_counts,
    )
    .await?;
    Ok((candidate, projection))
}

async fn plan_component_publication<S>(
    lease: ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
    component: ManagedComponentKind,
    sources: Vec<S>,
) -> Result<
    (
        ManagedRootPublicationLease,
        Vec<ComponentProjectionRow>,
        PlannedComponentProjection,
        BTreeMap<PortableRelativePath, S>,
        SparseSourceCounts,
    ),
    PrepareComponentIntentError,
>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let projection_rows = projection_rows(component, &projection)?;
    let sources = index_sparse_sources(&projection_rows, sources)?;
    let retained_projection = projection_rows.clone();
    run_publication_blocking(move || {
        let planned = plan_projection(&lease, component, projection_rows)?;
        let source_counts = validate_sparse_source_coverage(&planned.table_rows, &sources)?;
        Ok::<_, PrepareComponentIntentError>((
            lease,
            retained_projection,
            planned,
            sources,
            source_counts,
        ))
    })
    .await?
}

async fn prepare_component_intent_from_plan<S>(
    lease: ManagedRootPublicationLease,
    component: ManagedComponentKind,
    planned_rows: Vec<ComponentTableRow>,
    planned_authority: Vec<ComponentPreparedCanonicalAuthority>,
    sources: BTreeMap<PortableRelativePath, S>,
    source_counts: SparseSourceCounts,
) -> Result<ComponentIntentCandidate, PrepareComponentIntentError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let total_rows = planned_rows.len();
    let (mut lease, mut lane, mut builder, mut spool, mut sources) =
        run_publication_blocking(move || {
            let lane = ComponentLane::prepare_fresh(&lease, component)?;
            let root_binding_sha256 = component_root_binding_sha256(lease.root())?;
            let transaction_nonce = *uuid::Uuid::new_v4().as_bytes();
            let builder = ComponentTableBuilder::new(
                component,
                total_rows,
                transaction_nonce,
                root_binding_sha256,
            )?;
            let spool = ComponentTableSpool::new(total_rows)?;
            Ok::<_, PrepareComponentIntentError>((lease, lane, builder, spool, sources))
        })
        .await??;
    if planned_rows.len() != planned_authority.len() {
        return Err(PrepareComponentIntentError::SourceSet);
    }
    let mut planned_rows = VecDeque::from(planned_rows);
    let mut planned_authority = VecDeque::from(planned_authority);
    let mut staged_sources = 0_usize;
    let mut dropped_exact_sources = 0_usize;
    let mut shard_index = 0_usize;
    let mut prepared_shards = Vec::new();
    prepared_shards
        .try_reserve_exact(total_rows.div_ceil(COMPONENT_TABLE_ROWS_PER_SHARD))
        .map_err(|_| PrepareComponentIntentError::SourceSet)?;
    while !planned_rows.is_empty() {
        let shard_len = planned_rows.len().min(COMPONENT_TABLE_ROWS_PER_SHARD);
        let mut shard_rows = Vec::new();
        let mut shard_authority = Vec::new();
        shard_rows
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareComponentIntentError::SourceSet)?;
        shard_authority
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareComponentIntentError::SourceSet)?;
        for _ in 0..shard_len {
            shard_rows.push(
                planned_rows
                    .pop_front()
                    .ok_or(PrepareComponentIntentError::SourceSet)?,
            );
            shard_authority.push(
                planned_authority
                    .pop_front()
                    .ok_or(PrepareComponentIntentError::SourceSet)?,
            );
        }
        let prepared = run_publication_blocking(move || {
            let buckets = lane.create_shard_buckets(shard_index)?;
            Ok::<_, PrepareComponentIntentError>((lease, lane, buckets, shard_rows))
        })
        .await??;
        let (returned_lease, returned_lane, buckets, shard_rows) = prepared;
        lease = returned_lease;
        lane = returned_lane;
        let mut table_rows = Vec::new();
        table_rows
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareComponentIntentError::SourceSet)?;
        let mut prepared_rows = Vec::new();
        prepared_rows
            .try_reserve_exact(shard_len)
            .map_err(|_| PrepareComponentIntentError::SourceSet)?;
        for (row_in_shard, (row, authority)) in
            shard_rows.into_iter().zip(shard_authority).enumerate()
        {
            let source = sources.remove(&row.path);
            let staging = if row.prior_is_final() {
                if let Some(source) = source {
                    drop(source);
                    dropped_exact_sources += 1;
                }
                None
            } else {
                let staged = source
                    .ok_or(PrepareComponentIntentError::SourceSet)?
                    .stage_create_new(
                        buckets.staging(),
                        &component_slot_name(row_in_shard)?,
                        lease.lifetime_guard(),
                    )
                    .await?;
                if !staged.matches(&row.path, row.kind, row.final_size, row.final_sha1) {
                    return Err(PrepareComponentIntentError::SourceSet);
                }
                staged_sources += 1;
                Some(staged)
            };
            prepared_rows.push(
                authority.with_staging(staging.as_ref().map(|staged| staged.file_identity())),
            );
            table_rows.push(row);
        }
        prepared_shards.push(buckets.prepared_authority(prepared_rows)?);
        let pushed = run_publication_blocking(move || {
            let (encoded, descriptor) = builder.push_shard(table_rows)?;
            spool.append(encoded, descriptor)?;
            Ok::<_, PrepareComponentIntentError>((lease, lane, builder, spool))
        })
        .await??;
        (lease, lane, builder, spool) = pushed;
        shard_index += 1;
    }
    if !planned_authority.is_empty()
        || !sources.is_empty()
        || staged_sources != source_counts.required
        || dropped_exact_sources != source_counts.supplied_exact
    {
        return Err(PrepareComponentIntentError::SourceSet);
    }
    let candidate = run_publication_blocking(move || {
        let (manifest, summary) = builder.finish()?;
        let replay = spool.finish(&manifest)?;
        let (durable_summary, table_files) = lane.publish_table(replay, &manifest)?;
        validate_table_summary(&summary, &durable_summary, manifest.shards.len(), &manifest)?;
        Ok::<_, PrepareComponentIntentError>(lane.into_prepared_intent_candidate(
            lease,
            manifest,
            durable_summary,
            table_files,
            prepared_shards,
        )?)
    })
    .await??;
    Ok(candidate)
}

#[cfg(test)]
async fn prepare_component_intent<S>(
    lease: ManagedRootPublicationLease,
    authority: &PendingKnownGoodInstallAuthority,
    component: ManagedComponentKind,
    sources: Vec<S>,
) -> Result<ComponentIntentCandidate, PrepareComponentIntentError>
where
    S: RetainedComponentPublicationSource + 'static,
{
    let (known_good_component, _) = component_projection_contract(component);
    let projection = authority
        .component_projection(known_good_component)
        .map_err(|_| PrepareComponentIntentError::Projection)?;
    prepare_component_intent_projection(lease, projection, component, sources)
        .await
        .map(|(candidate, _)| candidate)
}

fn projection_rows(
    component: ManagedComponentKind,
    projection: &crate::known_good::ManagedComponentProjection<'_>,
) -> Result<Vec<ComponentProjectionRow>, PrepareComponentIntentError> {
    let (known_good_component, expected_root) = component_projection_contract(component);
    if projection.component() != known_good_component {
        return Err(PrepareComponentIntentError::Projection);
    }
    let mut rows = Vec::new();
    let mut portable_paths = BTreeMap::new();
    rows.try_reserve_exact(projection.entry_count())
        .map_err(|_| PrepareComponentIntentError::SourceSet)?;
    for projected in projection.entries().iter().copied() {
        let entry = projected.entry();
        if entry.root() != &expected_root {
            return Err(PrepareComponentIntentError::Projection);
        }
        let relative_path = PortableRelativePath::new(entry.path().as_str())
            .map_err(|_| PrepareComponentIntentError::Projection)?;
        let portable_path = relative_path.key();
        if portable_paths
            .insert(portable_path, relative_path.clone())
            .is_some()
        {
            return Err(PrepareComponentIntentError::Projection);
        }
        let kind = component_artifact_kind(component, entry.kind())?;
        let (sha1, size) = sha1_integrity(entry.integrity())?;
        rows.push(ComponentProjectionRow {
            inventory_ordinal: u32::try_from(projected.inventory_ordinal())
                .map_err(|_| PrepareComponentIntentError::Projection)?,
            path: relative_path,
            kind,
            size,
            sha1,
        });
    }
    Ok(rows)
}

fn index_sparse_sources<S>(
    projection: &[ComponentProjectionRow],
    sources: Vec<S>,
) -> Result<BTreeMap<PortableRelativePath, S>, PrepareComponentIntentError>
where
    S: RetainedComponentPublicationSource,
{
    let mut projected = BTreeMap::new();
    for row in projection {
        if projected.insert(row.path.clone(), row).is_some() {
            return Err(PrepareComponentIntentError::Projection);
        }
    }
    let mut indexed = BTreeMap::new();
    for source in sources {
        let relative_path = source.relative_path().clone();
        let expected = projected
            .get(&relative_path)
            .ok_or(PrepareComponentIntentError::SourceSet)?;
        if source.kind() != expected.kind
            || source.observed_size() != expected.size
            || source.observed_sha1() != expected.sha1
        {
            return Err(PrepareComponentIntentError::SourceSet);
        }
        if indexed.insert(relative_path, source).is_some() {
            return Err(PrepareComponentIntentError::SourceSet);
        }
    }
    Ok(indexed)
}

fn plan_projection(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    projection: Vec<ComponentProjectionRow>,
) -> Result<PlannedComponentProjection, PrepareComponentIntentError> {
    let mut rows = Vec::new();
    let mut authority = Vec::new();
    rows.try_reserve_exact(projection.len())
        .map_err(|_| PrepareComponentIntentError::SourceSet)?;
    authority
        .try_reserve_exact(projection.len())
        .map_err(|_| PrepareComponentIntentError::SourceSet)?;
    let mut all_exact = true;
    for projected in projection {
        let path_plan = plan_component_canonical_path(lease.root(), component, &projected.path)?;
        let first_created_depth = path_plan.first_created_depth();
        let (observed, prepared) = path_plan.observe_prepared()?;
        let prior = match observed {
            ComponentCanonicalObservation::Absent => {
                all_exact = false;
                None
            }
            ComponentCanonicalObservation::Regular(observed) => {
                let prior = Some(ComponentPriorFile {
                    size: observed.size(),
                    sha1: observed.sha1(),
                });
                all_exact &= observed.size() == projected.size && observed.sha1() == projected.sha1;
                prior
            }
        };
        rows.push(ComponentTableRow {
            inventory_ordinal: projected.inventory_ordinal,
            final_size: projected.size,
            final_sha1: projected.sha1,
            kind: projected.kind,
            path: projected.path,
            first_created_depth,
            prior,
        });
        authority.push(prepared);
    }
    lease.revalidate()?;
    Ok(PlannedComponentProjection {
        table_rows: rows,
        authority,
        all_exact,
    })
}

async fn revalidate_exact_component_projection(
    lease: ManagedRootPublicationLease,
    component: ManagedComponentKind,
    rows: Vec<ComponentTableRow>,
) -> Result<ManagedRootPublicationLease, PrepareComponentIntentError> {
    let lease = run_publication_blocking(move || {
        lease.revalidate()?;
        for row in rows {
            let planned = plan_component_canonical_path(lease.root(), component, &row.path)?;
            let ComponentCanonicalObservation::Regular(observed) = planned.observe()? else {
                return Err(PrepareComponentIntentError::CanonicalChanged);
            };
            if observed.size() != row.final_size || observed.sha1() != row.final_sha1 {
                return Err(PrepareComponentIntentError::CanonicalChanged);
            }
        }
        lease.revalidate()?;
        Ok::<_, PrepareComponentIntentError>(lease)
    })
    .await??;
    lease.revalidate()?;
    Ok(lease)
}

async fn revalidate_component_projection(
    lease: &ManagedRootPublicationLease,
    component: ManagedComponentKind,
    projection: &[ComponentProjectionRow],
) -> bool {
    if lease.revalidate().is_err() {
        return false;
    }
    let root = lease.root().clone();
    let projection = projection.to_vec();
    let exact = run_publication_blocking(move || {
        root.revalidate()?;
        for row in projection {
            let planned = plan_component_canonical_path(&root, component, &row.path)?;
            let ComponentCanonicalObservation::Regular(observed) = planned.observe()? else {
                return Ok::<_, ComponentEffectsError>(false);
            };
            if observed.size() != row.size || observed.sha1() != row.sha1 {
                return Ok(false);
            }
        }
        root.revalidate()?;
        Ok(true)
    })
    .await;
    matches!(exact, Ok(Ok(true))) && lease.revalidate().is_ok()
}

fn validate_sparse_source_coverage<S>(
    planned: &[ComponentTableRow],
    sources: &BTreeMap<PortableRelativePath, S>,
) -> Result<SparseSourceCounts, PrepareComponentIntentError>
where
    S: RetainedComponentPublicationSource,
{
    let mut counts = SparseSourceCounts {
        required: 0,
        supplied_exact: 0,
    };
    for row in planned {
        if row.prior_is_final() {
            if sources.contains_key(&row.path) {
                counts.supplied_exact += 1;
            }
        } else {
            if !sources.contains_key(&row.path) {
                return Err(PrepareComponentIntentError::SourceSet);
            }
            counts.required += 1;
        }
    }
    if sources.len() != counts.required + counts.supplied_exact {
        return Err(PrepareComponentIntentError::SourceSet);
    }
    Ok(counts)
}

fn component_projection_contract(
    component: ManagedComponentKind,
) -> (ManagedKnownGoodComponent, KnownGoodRoot) {
    match component {
        ManagedComponentKind::Libraries => (
            ManagedKnownGoodComponent::Libraries,
            KnownGoodRoot::Libraries,
        ),
        ManagedComponentKind::Assets => (ManagedKnownGoodComponent::Assets, KnownGoodRoot::Assets),
    }
}

fn component_artifact_kind(
    component: ManagedComponentKind,
    kind: KnownGoodArtifactKind,
) -> Result<ManagedComponentArtifactKind, PrepareComponentIntentError> {
    match (component, kind) {
        (ManagedComponentKind::Libraries, KnownGoodArtifactKind::Library) => {
            Ok(ManagedComponentArtifactKind::Library)
        }
        (ManagedComponentKind::Libraries, KnownGoodArtifactKind::NativeLibrary) => {
            Ok(ManagedComponentArtifactKind::NativeLibrary)
        }
        (ManagedComponentKind::Assets, KnownGoodArtifactKind::AssetIndex) => {
            Ok(ManagedComponentArtifactKind::AssetIndex)
        }
        (ManagedComponentKind::Assets, KnownGoodArtifactKind::AssetObject) => {
            Ok(ManagedComponentArtifactKind::AssetObject)
        }
        (ManagedComponentKind::Libraries | ManagedComponentKind::Assets, _) => {
            Err(PrepareComponentIntentError::Projection)
        }
    }
}

fn sha1_integrity(
    integrity: &KnownGoodIntegrity,
) -> Result<([u8; 20], u64), PrepareComponentIntentError> {
    match integrity {
        KnownGoodIntegrity::Sha1 { digest, size } => Ok((digest.to_bytes(), *size)),
        KnownGoodIntegrity::ExactBytes { .. }
        | KnownGoodIntegrity::Directory
        | KnownGoodIntegrity::LinkTarget(_) => Err(PrepareComponentIntentError::Projection),
    }
}

fn validate_table_summary(
    built: &ComponentTableSummary,
    durable: &ComponentTableSummary,
    durable_shards: usize,
    manifest: &ComponentIntentManifest,
) -> Result<(), PrepareComponentIntentError> {
    if built != durable || durable_shards != manifest.shards.len() {
        return Err(PrepareComponentIntentError::TableSummary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_component_publication::COMPONENT_INTENT_FILE;
    use crate::managed_component_table::decode_component_table_shard;
    use crate::managed_fs::{register_sha1_full_read_counts, take_sha1_full_read_counts};
    use sha1::{Digest as _, Sha1};
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestSourceEvents {
        staged: AtomicUsize,
        dropped: AtomicUsize,
    }

    #[derive(Clone)]
    struct TestSource {
        identity: ComponentPublicationSourceIdentity,
        bytes: Vec<u8>,
        events: Option<Arc<TestSourceEvents>>,
    }

    impl Drop for TestSource {
        fn drop(&mut self) {
            if let Some(events) = &self.events {
                events.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl RetainedComponentPublicationSource for TestSource {
        fn relative_path(&self) -> &PortableRelativePath {
            &self.identity.relative_path
        }

        fn kind(&self) -> ManagedComponentArtifactKind {
            self.identity.kind
        }

        fn observed_size(&self) -> u64 {
            self.identity.size
        }

        fn observed_sha1(&self) -> [u8; 20] {
            self.identity.sha1
        }

        async fn stage_create_new(
            self,
            staging_bucket: &ManagedDir,
            slot: &str,
            lifetime_guard: ManagedPublicationLifetimeGuard,
        ) -> Result<StagedComponentPublicationSource, LoaderError> {
            if let Some(events) = &self.events {
                events.staged.fetch_add(1, Ordering::SeqCst);
            }
            let file = staging_bucket
                .import_authenticated_create_new(
                    slot,
                    std::io::Cursor::new(self.bytes.clone()),
                    self.identity.size,
                    self.identity.sha1,
                    lifetime_guard,
                )
                .await?;
            Ok(StagedComponentPublicationSource::new(
                self.identity.clone(),
                file,
            ))
        }
    }

    fn test_source(
        path: &str,
        kind: ManagedComponentArtifactKind,
        bytes: impl Into<Vec<u8>>,
    ) -> TestSource {
        let bytes = bytes.into();
        TestSource {
            identity: ComponentPublicationSourceIdentity::new(
                PortableRelativePath::new(path).expect("test source path"),
                kind,
                u64::try_from(bytes.len()).expect("test source size"),
                Sha1::digest(&bytes).into(),
            ),
            bytes,
            events: None,
        }
    }

    fn tracked_test_source(
        path: &str,
        kind: ManagedComponentArtifactKind,
        bytes: impl Into<Vec<u8>>,
    ) -> (TestSource, Arc<TestSourceEvents>) {
        let mut source = test_source(path, kind, bytes);
        let events = Arc::new(TestSourceEvents::default());
        source.events = Some(Arc::clone(&events));
        (source, events)
    }

    fn test_authority(
        component: ManagedComponentKind,
        sources: &[TestSource],
    ) -> PendingKnownGoodInstallAuthority {
        let root = match component {
            ManagedComponentKind::Libraries => KnownGoodRoot::Libraries,
            ManagedComponentKind::Assets => KnownGoodRoot::Assets,
        };
        PendingKnownGoodInstallAuthority::component_for_test(sources.iter().map(|source| {
            (
                root.clone(),
                source.identity.relative_path.as_str().to_string(),
                match source.identity.kind {
                    ManagedComponentArtifactKind::Library => KnownGoodArtifactKind::Library,
                    ManagedComponentArtifactKind::NativeLibrary => {
                        KnownGoodArtifactKind::NativeLibrary
                    }
                    ManagedComponentArtifactKind::AssetIndex => KnownGoodArtifactKind::AssetIndex,
                    ManagedComponentArtifactKind::AssetObject => KnownGoodArtifactKind::AssetObject,
                },
                source.identity.sha1,
                source.identity.size,
            )
        }))
    }

    async fn test_lease(temporary: &tempfile::TempDir) -> ManagedRootPublicationLease {
        let root = ManagedDir::open_root(temporary.path()).expect("test managed root");
        ManagedRootPublicationLease::acquire(root)
            .await
            .expect("test publication lease")
    }

    async fn prepared_mixed_candidate(
        temporary: &tempfile::TempDir,
    ) -> (ComponentIntentCandidate, PortableRelativePath, Vec<u8>) {
        let inherited_bytes = b"inherited-authority".to_vec();
        let inherited = test_source(
            "org/example/inherited.jar",
            ManagedComponentArtifactKind::Library,
            inherited_bytes.clone(),
        );
        let fresh = test_source(
            "org/example/fresh.jar",
            ManagedComponentArtifactKind::NativeLibrary,
            b"fresh-authority".to_vec(),
        );
        let inherited_path = inherited.identity.relative_path.clone();
        let canonical = inherited_path.join_under(&temporary.path().join("libraries"));
        fs::create_dir_all(canonical.parent().expect("canonical parent")).unwrap();
        fs::write(&canonical, &inherited_bytes).unwrap();
        let authority =
            test_authority(ManagedComponentKind::Libraries, &[inherited, fresh.clone()]);
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("mixed projection");
        let (candidate, _) = prepare_component_intent_projection(
            test_lease(temporary).await,
            projection,
            ManagedComponentKind::Libraries,
            vec![fresh],
        )
        .await
        .expect("prepared mixed candidate");
        (candidate, inherited_path, inherited_bytes)
    }

    fn observed_identity(
        temporary: &tempfile::TempDir,
        path: &PortableRelativePath,
    ) -> ManagedFileIdentity {
        let root = ManagedDir::open_root(temporary.path()).expect("managed test root");
        let plan = plan_component_canonical_path(&root, ManagedComponentKind::Libraries, path)
            .expect("canonical plan");
        let ComponentCanonicalObservation::Regular(observed) =
            plan.observe().expect("canonical observation")
        else {
            panic!("canonical test file must exist")
        };
        observed.guard().identity()
    }

    fn component_root_name(component: ManagedComponentKind) -> &'static str {
        match component {
            ManagedComponentKind::Libraries => "libraries",
            ManagedComponentKind::Assets => "assets",
        }
    }

    fn component_test_kinds(
        component: ManagedComponentKind,
    ) -> (ManagedComponentArtifactKind, ManagedComponentArtifactKind) {
        match component {
            ManagedComponentKind::Libraries => (
                ManagedComponentArtifactKind::Library,
                ManagedComponentArtifactKind::NativeLibrary,
            ),
            ManagedComponentKind::Assets => (
                ManagedComponentArtifactKind::AssetIndex,
                ManagedComponentArtifactKind::AssetObject,
            ),
        }
    }

    fn assert_component_lane_settled(
        temporary: &tempfile::TempDir,
        component: ManagedComponentKind,
    ) {
        let lane = temporary
            .path()
            .join(".axial-publication")
            .join(component_root_name(component));
        for directory in [
            "table",
            "staging",
            "quarantine",
            "ancestors/records",
            "ancestors/staging",
        ] {
            assert!(
                fs::read_dir(lane.join(directory)).unwrap().next().is_none(),
                "settled component directory {directory} retained residue"
            );
        }
        for marker in ["intent.bin", "outcome.bin", "settlement.bin"] {
            assert!(!lane.join(marker).exists());
        }
    }

    fn assert_component_lane_absent(
        temporary: &tempfile::TempDir,
        component: ManagedComponentKind,
    ) {
        assert!(
            !temporary
                .path()
                .join(".axial-publication")
                .join(component_root_name(component))
                .exists(),
            "exact component projection created a publication lane"
        );
    }

    fn assert_no_component_lanes(temporary: &tempfile::TempDir) {
        for name in ["libraries", "assets"] {
            assert!(
                !temporary
                    .path()
                    .join(".axial-publication")
                    .join(name)
                    .exists(),
                "component lane {name} was created before admission"
            );
            assert!(
                !temporary.path().join(name).exists(),
                "canonical component root {name} was created before admission"
            );
        }
        let publication_entries = fs::read_dir(temporary.path().join(".axial-publication"))
            .expect("publication root")
            .map(|entry| entry.expect("publication entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            publication_entries,
            [std::ffi::OsString::from("publication.lock")]
        );
    }

    #[tokio::test]
    async fn exact_projection_drops_supplied_source_without_creating_a_lane() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let (kind, _) = component_test_kinds(component);
            let path = match component {
                ManagedComponentKind::Libraries => "org/example/exact.jar",
                ManagedComponentKind::Assets => "indexes/exact.json",
            };
            let bytes = format!("exact-{}", component_root_name(component)).into_bytes();
            let (source, events) = tracked_test_source(path, kind, bytes.clone());
            let authority = test_authority(component, std::slice::from_ref(&source));
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join(component_root_name(component)));
            fs::create_dir_all(canonical.parent().expect("canonical parent")).unwrap();
            fs::write(&canonical, &bytes).unwrap();
            let mut faults = ComponentLifecycleTestFaults::default();

            let outcome = publish_managed_component_with_faults(
                test_lease(&temporary).await,
                &authority,
                component,
                vec![source],
                &mut faults,
            )
            .await
            .expect("exact component no-effect publication");

            assert!(matches!(
                outcome,
                ManagedComponentLifecycleOutcome::Committed(_)
            ));
            assert_eq!(events.staged.load(Ordering::SeqCst), 0);
            assert_eq!(events.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(fs::read(canonical).unwrap(), bytes);
            assert_component_lane_absent(&temporary, component);
        }
    }

    #[tokio::test]
    async fn large_exact_projection_streams_without_a_publication_lane() {
        const ROWS: usize = 384;

        let temporary = tempfile::tempdir().expect("test root");
        let sources = (0..ROWS)
            .map(|index| {
                test_source(
                    &format!("org/example/{index:03}.jar"),
                    ManagedComponentArtifactKind::Library,
                    format!("exact-{index:03}").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        for source in &sources {
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join("libraries"));
            fs::create_dir_all(canonical.parent().expect("canonical parent")).unwrap();
            fs::write(canonical, &source.bytes).unwrap();
        }
        let authority = test_authority(ManagedComponentKind::Libraries, &sources);
        let mut faults = ComponentLifecycleTestFaults::default();

        let outcome = publish_managed_component_with_faults::<TestSource>(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            Vec::new(),
            &mut faults,
        )
        .await
        .expect("large exact projection must stream file guards");

        assert!(matches!(
            outcome,
            ManagedComponentLifecycleOutcome::Committed(_)
        ));
        assert_component_lane_absent(&temporary, ManagedComponentKind::Libraries);
    }

    #[tokio::test]
    async fn empty_projection_commits_without_creating_a_lane() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let other_component = match component {
                ManagedComponentKind::Libraries => ManagedComponentKind::Assets,
                ManagedComponentKind::Assets => ManagedComponentKind::Libraries,
            };
            let (other_kind, _) = component_test_kinds(other_component);
            let other_source = test_source(
                match other_component {
                    ManagedComponentKind::Libraries => "org/example/other.jar",
                    ManagedComponentKind::Assets => "indexes/other.json",
                },
                other_kind,
                b"other-component".to_vec(),
            );
            let authority = test_authority(other_component, &[other_source]);
            let mut faults = ComponentLifecycleTestFaults::default();

            let outcome = publish_managed_component_with_faults::<TestSource>(
                test_lease(&temporary).await,
                &authority,
                component,
                Vec::new(),
                &mut faults,
            )
            .await
            .expect("empty projection no-effect publication");

            assert!(matches!(
                outcome,
                ManagedComponentLifecycleOutcome::Committed(_)
            ));
            assert_component_lane_absent(&temporary, component);
        }
    }

    #[tokio::test]
    async fn exact_projection_rejects_same_size_mutation_between_hash_passes() {
        let temporary = tempfile::tempdir().expect("test root");
        let source = test_source(
            "org/example/exact.jar",
            ManagedComponentArtifactKind::Library,
            b"expected".to_vec(),
        );
        let authority = test_authority(
            ManagedComponentKind::Libraries,
            std::slice::from_ref(&source),
        );
        let canonical = source
            .identity
            .relative_path
            .join_under(&temporary.path().join("libraries"));
        fs::create_dir_all(canonical.parent().expect("canonical parent")).unwrap();
        fs::write(&canonical, b"expected").unwrap();
        let mutation_path = canonical.clone();
        let mut faults = ComponentLifecycleTestFaults {
            no_effect_before_final_validation: Some(Box::new(move || {
                fs::write(mutation_path, b"mutated!").expect("same-size mutation");
            })),
            ..ComponentLifecycleTestFaults::default()
        };

        let error = match publish_managed_component_with_faults::<TestSource>(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            Vec::new(),
            &mut faults,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("second guarded hash must reject same-size mutation"),
        };

        assert!(matches!(
            error,
            ComponentLifecycleError::Prepare(PrepareComponentIntentError::CanonicalChanged)
        ));
        assert_eq!(fs::read(canonical).unwrap(), b"mutated!");
        assert_component_lane_absent(&temporary, ManagedComponentKind::Libraries);
    }

    #[tokio::test]
    async fn exact_projection_settles_prior_transaction_before_no_effect_return() {
        let temporary = tempfile::tempdir().expect("test root");
        let source = test_source(
            "org/example/exact.jar",
            ManagedComponentArtifactKind::Library,
            b"expected".to_vec(),
        );
        let authority = test_authority(
            ManagedComponentKind::Libraries,
            std::slice::from_ref(&source),
        );
        let prior_candidate = prepare_component_intent(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            vec![source.clone()],
        )
        .await
        .expect("prepare prior transaction");
        let prior_published = prior_candidate
            .publish_intent()
            .unwrap_or_else(|_| panic!("publish prior transaction"));
        let ComponentExecutionResult::Committed(prior_receipt) =
            execute_component_intent(prior_published).await
        else {
            panic!("prior transaction must commit")
        };
        let (lease, component) = prior_receipt.into_restart_seed();
        assert_eq!(component, ManagedComponentKind::Libraries);
        let lane = temporary.path().join(".axial-publication/libraries");
        assert!(lane.join("outcome.bin").exists());
        let reached_no_effect = Arc::new(AtomicUsize::new(0));
        let hook_counter = Arc::clone(&reached_no_effect);
        let hook_lane = lane.clone();
        let mut faults = ComponentLifecycleTestFaults {
            no_effect_before_final_validation: Some(Box::new(move || {
                assert!(!hook_lane.join("outcome.bin").exists());
                assert!(!hook_lane.join("settlement.bin").exists());
                hook_counter.fetch_add(1, Ordering::SeqCst);
            })),
            ..ComponentLifecycleTestFaults::default()
        };

        let outcome = publish_managed_component_with_faults::<TestSource>(
            lease,
            &authority,
            ManagedComponentKind::Libraries,
            Vec::new(),
            &mut faults,
        )
        .await
        .expect("settle prior transaction before exact no-effect return");

        assert!(matches!(
            outcome,
            ManagedComponentLifecycleOutcome::Committed(_)
        ));
        assert_eq!(reached_no_effect.load(Ordering::SeqCst), 1);
        assert_component_lane_settled(&temporary, ManagedComponentKind::Libraries);
    }

    #[tokio::test]
    async fn lifecycle_recovers_attempted_intent_and_retries_committed_settlement() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let (kind, _) = component_test_kinds(component);
            let path = match component {
                ManagedComponentKind::Libraries => "org/example/current.jar",
                ManagedComponentKind::Assets => "indexes/current.json",
            };
            let bytes = format!("current-{}", component_root_name(component)).into_bytes();
            let source = test_source(path, kind, bytes.clone());
            let authority = test_authority(component, std::slice::from_ref(&source));
            let mut faults = ComponentLifecycleTestFaults {
                intent: Some(ComponentIntentPublishFault::PromotionAttemptedWithoutMarker),
                settlement: Some(ComponentSettlementFault::AfterSettlementPromotion),
                ..ComponentLifecycleTestFaults::default()
            };

            let outcome = publish_managed_component_with_faults(
                test_lease(&temporary).await,
                &authority,
                component,
                vec![source],
                &mut faults,
            )
            .await
            .expect("recover and settle current component transaction");
            let ManagedComponentLifecycleOutcome::Committed(receipt) = outcome else {
                panic!("recovered transaction must commit");
            };
            let lease = receipt.into_lease();

            lease.revalidate().expect("returned publication lease");
            assert_eq!(
                fs::read(
                    temporary
                        .path()
                        .join(component_root_name(component))
                        .join(path),
                )
                .unwrap(),
                bytes
            );
            assert!(faults.intent.is_none());
            assert!(faults.settlement.is_none());
            assert_component_lane_settled(&temporary, component);
            let other = match component {
                ManagedComponentKind::Libraries => "assets",
                ManagedComponentKind::Assets => "libraries",
            };
            assert!(!temporary.path().join(other).exists());
        }
    }

    #[tokio::test]
    async fn lifecycle_settles_current_rollback_as_typed_outcome() {
        let temporary = tempfile::tempdir().expect("test root");
        let source = test_source(
            "org/example/rollback.jar",
            ManagedComponentArtifactKind::Library,
            b"rollback-library".to_vec(),
        );
        let authority = test_authority(
            ManagedComponentKind::Libraries,
            std::slice::from_ref(&source),
        );
        let mut faults = ComponentLifecycleTestFaults {
            execution: Some(ComponentExecutionFault::AfterFirstRow),
            ..ComponentLifecycleTestFaults::default()
        };

        let outcome = publish_managed_component_with_faults(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            vec![source],
            &mut faults,
        )
        .await
        .expect("current rollback must settle");

        assert!(matches!(
            outcome,
            ManagedComponentLifecycleOutcome::RolledBack(_)
        ));
        assert!(!temporary.path().join("libraries").exists());
        assert!(faults.execution.is_none());
        assert_component_lane_settled(&temporary, ManagedComponentKind::Libraries);
    }

    #[tokio::test]
    async fn lifecycle_retries_crash_recovery_and_rolled_back_settlement() {
        let temporary = tempfile::tempdir().expect("test root");
        let sources = vec![
            test_source(
                "org/example/0.jar",
                ManagedComponentArtifactKind::Library,
                b"first-library".to_vec(),
            ),
            test_source(
                "org/example/1.jar",
                ManagedComponentArtifactKind::Library,
                b"second-library".to_vec(),
            ),
        ];
        let authority = test_authority(ManagedComponentKind::Libraries, &sources);
        let mut faults = ComponentLifecycleTestFaults {
            execution: Some(ComponentExecutionFault::CrashAfterFirstRow),
            settlement: Some(ComponentSettlementFault::AfterSettlementPromotion),
            ..ComponentLifecycleTestFaults::default()
        };

        let outcome = publish_managed_component_with_faults(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            sources,
            &mut faults,
        )
        .await
        .expect("reconciled current rollback must settle");

        assert!(matches!(
            outcome,
            ManagedComponentLifecycleOutcome::RolledBack(_)
        ));
        assert!(!temporary.path().join("libraries").exists());
        assert!(faults.execution.is_none());
        assert!(faults.settlement.is_none());
        assert_component_lane_settled(&temporary, ManagedComponentKind::Libraries);
    }

    #[tokio::test]
    async fn lifecycle_classifies_before_promotion_as_preeffect() {
        let temporary = tempfile::tempdir().expect("test root");
        let source = test_source(
            "org/example/preeffect.jar",
            ManagedComponentArtifactKind::Library,
            b"preeffect-library".to_vec(),
        );
        let authority = test_authority(
            ManagedComponentKind::Libraries,
            std::slice::from_ref(&source),
        );
        let mut faults = ComponentLifecycleTestFaults {
            intent: Some(ComponentIntentPublishFault::BeforeMarkerPromotion),
            ..ComponentLifecycleTestFaults::default()
        };

        let error = match publish_managed_component_with_faults(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            vec![source],
            &mut faults,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("before-promotion failure must not enter recovery"),
        };

        assert!(matches!(error, ComponentLifecycleError::BeforeEffects(_)));
        assert!(!temporary.path().join("libraries").exists());
        assert!(
            !temporary
                .path()
                .join(".axial-publication/libraries/intent.bin")
                .exists()
        );
    }

    #[tokio::test]
    async fn lifecycle_settles_different_prior_commit_before_current_replacement() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let (kind, _) = component_test_kinds(component);
            let path = match component {
                ManagedComponentKind::Libraries => "org/example/replaced.jar",
                ManagedComponentKind::Assets => "indexes/replaced.json",
            };
            let prior = test_source(path, kind, b"prior-component".to_vec());
            let prior_authority = test_authority(component, std::slice::from_ref(&prior));
            let prior_candidate = prepare_component_intent(
                test_lease(&temporary).await,
                &prior_authority,
                component,
                vec![prior],
            )
            .await
            .expect("prepare prior transaction");
            let prior_published = prior_candidate
                .publish_intent()
                .unwrap_or_else(|_| panic!("publish prior transaction"));
            let ComponentExecutionResult::Committed(prior_receipt) =
                execute_component_intent(prior_published).await
            else {
                panic!("prior transaction must reach committed outcome")
            };
            let (lease, observed_component) = prior_receipt.into_restart_seed();
            assert_eq!(observed_component, component);

            let current = test_source(path, kind, b"current-component".to_vec());
            let current_authority = test_authority(component, std::slice::from_ref(&current));
            let (known_good_component, _) = component_projection_contract(component);
            let projection = current_authority
                .component_projection(known_good_component)
                .expect("current component projection");
            let outcome =
                publish_managed_component_effect(lease, projection, component, vec![current])
                    .await
                    .expect("settle prior and publish current transaction");
            let ManagedComponentLifecycleOutcome::Committed(receipt) = outcome else {
                panic!("current replacement must commit");
            };
            let lease = receipt.into_lease();

            lease.revalidate().expect("returned current lease");
            assert_eq!(
                fs::read(
                    temporary
                        .path()
                        .join(component_root_name(component))
                        .join(path),
                )
                .unwrap(),
                b"current-component"
            );
            assert_component_lane_settled(&temporary, component);
        }
    }

    #[tokio::test]
    async fn sparse_sources_keep_projection_slots_across_shard_boundary() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = (0_usize..257)
            .map(|index| {
                test_source(
                    &format!("org/example/{index:03}.jar"),
                    if index.is_multiple_of(2) {
                        ManagedComponentArtifactKind::Library
                    } else {
                        ManagedComponentArtifactKind::NativeLibrary
                    },
                    format!("source-{index:03}").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        for source in &expected[..255] {
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join("libraries"));
            fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            fs::write(canonical, &source.bytes).unwrap();
        }
        let authority = test_authority(ManagedComponentKind::Libraries, &expected);
        let sparse = vec![expected[256].clone(), expected[255].clone()];
        let candidate = prepare_component_intent(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            sparse,
        )
        .await
        .expect("prepared sparse Libraries intent candidate");
        let lane = temporary.path().join(".axial-publication/libraries");

        assert!(!lane.join("staging/000000/000").exists());
        assert!(lane.join("staging/000000/255").is_file());
        assert!(lane.join("staging/000001/000").is_file());
        assert_eq!(
            fs::read_dir(lane.join("staging/000000")).unwrap().count(),
            1
        );
        assert_eq!(
            fs::read_dir(lane.join("staging/000001")).unwrap().count(),
            1
        );
        assert!(lane.join("table/000000.tbl").is_file());
        assert!(lane.join("table/000001.tbl").is_file());
        assert!(!lane.join(COMPONENT_INTENT_FILE).exists());

        let prepared = candidate
            .publish_intent()
            .unwrap_or_else(|_| panic!("durable Libraries intent"));
        assert!(lane.join(COMPONENT_INTENT_FILE).is_file());
        drop(prepared);
    }

    #[tokio::test]
    async fn supplied_exact_source_is_dropped_while_replacement_is_staged() {
        let temporary = tempfile::tempdir().expect("test root");
        let (exact, exact_events) = tracked_test_source(
            "org/example/a.jar",
            ManagedComponentArtifactKind::Library,
            b"exact-prior".to_vec(),
        );
        let (replacement, replacement_events) = tracked_test_source(
            "org/example/b.jar",
            ManagedComponentArtifactKind::NativeLibrary,
            b"replacement".to_vec(),
        );
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/a.jar"),
            &exact.bytes,
        )
        .unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/b.jar"),
            b"wrong-prior",
        )
        .unwrap();
        let sources = vec![exact, replacement];
        let authority = test_authority(ManagedComponentKind::Libraries, &sources);
        let candidate = prepare_component_intent(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            sources,
        )
        .await
        .expect("prepared mixed-prior candidate");
        let lane = temporary.path().join(".axial-publication/libraries");
        let staging = lane.join("staging/000000");
        let quarantine = lane.join("quarantine/000000");
        let shard = decode_component_table_shard(
            &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
        )
        .expect("decoded table shard");

        assert!(!staging.join("000").exists());
        assert!(staging.join("001").is_file());
        assert_eq!(fs::read_dir(quarantine).unwrap().count(), 0);
        assert!(shard.rows[0].prior_is_final());
        assert!(!shard.rows[1].prior_is_final());
        assert_eq!(shard.rows[0].first_created_depth, None);
        assert_eq!(shard.rows[1].first_created_depth, None);
        assert_eq!(exact_events.staged.load(Ordering::SeqCst), 0);
        assert_eq!(exact_events.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.staged.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.dropped.load(Ordering::SeqCst), 1);
        drop(candidate);
    }

    #[tokio::test]
    async fn exact_inherited_projection_prepares_with_zero_sources() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = vec![
            test_source(
                "org/example/a.jar",
                ManagedComponentArtifactKind::Library,
                b"exact-a".to_vec(),
            ),
            test_source(
                "org/example/b.jar",
                ManagedComponentArtifactKind::NativeLibrary,
                b"exact-b".to_vec(),
            ),
        ];
        for source in &expected {
            let canonical = source
                .identity
                .relative_path
                .join_under(&temporary.path().join("libraries"));
            fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            fs::write(canonical, &source.bytes).unwrap();
        }
        let authority = test_authority(ManagedComponentKind::Libraries, &expected);

        let candidate = prepare_component_intent::<TestSource>(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            Vec::new(),
        )
        .await
        .expect("exact inherited rows need no retained sources");
        let lane = temporary.path().join(".axial-publication/libraries");
        let shard = decode_component_table_shard(
            &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
        )
        .expect("decoded table shard");

        assert!(
            fs::read_dir(lane.join("staging/000000"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(shard.rows.iter().all(ComponentTableRow::prior_is_final));
        drop(candidate);
    }

    #[tokio::test]
    async fn missing_non_exact_source_is_rejected_before_lane_creation() {
        let temporary = tempfile::tempdir().expect("test root");
        let expected = vec![test_source(
            "org/example/missing.jar",
            ManagedComponentArtifactKind::Library,
            b"required-source".to_vec(),
        )];
        let authority = test_authority(ManagedComponentKind::Libraries, &expected);

        let error = match prepare_component_intent::<TestSource>(
            test_lease(&temporary).await,
            &authority,
            ManagedComponentKind::Libraries,
            Vec::new(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("missing non-exact source was accepted"),
        };

        assert!(matches!(error, PrepareComponentIntentError::SourceSet));
        assert!(
            !temporary
                .path()
                .join(".axial-publication/libraries")
                .exists()
        );
    }

    #[tokio::test]
    async fn sparse_mixed_projection_stages_only_non_exact_sources() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let (first_kind, second_kind) = component_test_kinds(component);
            let paths = match component {
                ManagedComponentKind::Libraries => [
                    "org/example/a.jar",
                    "org/example/b.jar",
                    "org/example/c.jar",
                ],
                ManagedComponentKind::Assets => {
                    ["indexes/a.json", "objects/ab/ab01", "objects/cd/cd02"]
                }
            };
            let expected = vec![
                test_source(paths[0], first_kind, b"exact-a".to_vec()),
                test_source(paths[1], second_kind, b"replacement-b".to_vec()),
                test_source(paths[2], second_kind, b"missing-c".to_vec()),
            ];
            let canonical_root = temporary.path().join(component_root_name(component));
            for (path, bytes) in [
                (paths[0], expected[0].bytes.as_slice()),
                (paths[1], b"wrong-b".as_slice()),
            ] {
                let canonical = canonical_root.join(path);
                fs::create_dir_all(canonical.parent().unwrap()).unwrap();
                fs::write(canonical, bytes).unwrap();
            }
            let authority = test_authority(component, &expected);
            let sparse = vec![expected[2].clone(), expected[1].clone()];

            let candidate = prepare_component_intent(
                test_lease(&temporary).await,
                &authority,
                component,
                sparse,
            )
            .await
            .expect("sparse mixed component projection");
            let lane = temporary
                .path()
                .join(".axial-publication")
                .join(component_root_name(component));
            let staging = lane.join("staging/000000");
            let shard = decode_component_table_shard(
                &fs::read(lane.join("table/000000.tbl")).expect("durable table shard"),
            )
            .expect("decoded table shard");

            assert!(!staging.join("000").exists());
            assert_eq!(fs::read(staging.join("001")).unwrap(), b"replacement-b");
            assert_eq!(fs::read(staging.join("002")).unwrap(), b"missing-c");
            assert!(shard.rows[0].prior_is_final());
            assert!(!shard.rows[1].prior_is_final());
            assert!(shard.rows[2].prior.is_none());
            drop(candidate);
        }
    }

    #[tokio::test]
    async fn mixed_component_publication_has_measured_target_hash_budget() {
        let temporary = tempfile::tempdir().expect("test root");
        let inherited = test_source(
            "org/example/inherited.jar",
            ManagedComponentArtifactKind::Library,
            b"inherited-exact-component".to_vec(),
        );
        let fresh = test_source(
            "org/example/fresh.jar",
            ManagedComponentArtifactKind::NativeLibrary,
            b"fresh-component-with-distinct-content".to_vec(),
        );
        let inherited_identity = inherited.identity.clone();
        let fresh_identity = fresh.identity.clone();
        let canonical = inherited
            .identity
            .relative_path
            .join_under(&temporary.path().join("libraries"));
        fs::create_dir_all(canonical.parent().expect("canonical parent")).unwrap();
        fs::write(&canonical, &inherited.bytes).unwrap();
        let authority = test_authority(
            ManagedComponentKind::Libraries,
            &[inherited.clone(), fresh.clone()],
        );
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::Libraries)
            .expect("mixed component projection");
        let lease = test_lease(&temporary).await;
        register_sha1_full_read_counts(temporary.path());

        let outcome = publish_managed_component_effect(
            lease,
            projection,
            ManagedComponentKind::Libraries,
            vec![fresh],
        )
        .await
        .expect("mixed component publication");

        assert!(matches!(
            outcome,
            ManagedComponentLifecycleOutcome::Committed(_)
        ));
        assert_eq!(fs::read(canonical).unwrap(), inherited.bytes);
        assert_eq!(
            fs::read(
                fresh_identity
                    .relative_path
                    .join_under(&temporary.path().join("libraries"))
            )
            .unwrap(),
            b"fresh-component-with-distinct-content"
        );
        assert_component_lane_settled(&temporary, ManagedComponentKind::Libraries);
        let counts = take_sha1_full_read_counts(temporary.path());
        assert_eq!(
            counts.count(inherited_identity.size, inherited_identity.sha1),
            8
        );
        assert_eq!(counts.count(fresh_identity.size, fresh_identity.sha1), 9);
    }

    #[tokio::test]
    async fn prepared_candidate_rejects_same_identity_same_size_content_mutation() {
        let temporary = tempfile::tempdir().expect("test root");
        let (candidate, inherited_path, inherited_bytes) =
            prepared_mixed_candidate(&temporary).await;
        let canonical = inherited_path.join_under(&temporary.path().join("libraries"));
        let before = observed_identity(&temporary, &inherited_path);
        let mutated = vec![b'x'; inherited_bytes.len()];

        fs::write(&canonical, &mutated).expect("mutate canonical in place");
        assert_eq!(observed_identity(&temporary, &inherited_path), before);

        assert!(matches!(
            candidate.publish_intent(),
            Err(ComponentIntentPublishFailure::BeforePromotion { .. })
        ));
        assert!(
            !temporary
                .path()
                .join(".axial-publication/libraries/intent.bin")
                .exists()
        );
        assert_eq!(fs::read(canonical).unwrap(), mutated);
    }

    #[tokio::test]
    async fn prepared_candidate_rejects_exact_content_identity_replacement() {
        let temporary = tempfile::tempdir().expect("test root");
        let (candidate, inherited_path, inherited_bytes) =
            prepared_mixed_candidate(&temporary).await;
        let canonical = inherited_path.join_under(&temporary.path().join("libraries"));
        let displaced = canonical.with_extension("saved");
        let before = observed_identity(&temporary, &inherited_path);

        fs::rename(&canonical, &displaced).expect("retain original identity");
        fs::write(&canonical, &inherited_bytes).expect("replace canonical with exact bytes");
        assert_ne!(observed_identity(&temporary, &inherited_path), before);

        assert!(matches!(
            candidate.publish_intent(),
            Err(ComponentIntentPublishFailure::BeforePromotion { .. })
        ));
        assert!(
            !temporary
                .path()
                .join(".axial-publication/libraries/intent.bin")
                .exists()
        );
        assert_eq!(fs::read(canonical).unwrap(), inherited_bytes);
        assert!(displaced.is_file());
    }

    #[tokio::test]
    async fn zero_byte_asset_source_uses_authenticated_create_new_staging() {
        let temporary = tempfile::tempdir().expect("test root");
        let source = test_source(
            "objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709",
            ManagedComponentArtifactKind::AssetObject,
            Vec::new(),
        );
        let authority = test_authority(ManagedComponentKind::Assets, std::slice::from_ref(&source));

        let projection = authority
            .component_projection(ManagedKnownGoodComponent::Assets)
            .expect("Assets projection");
        let outcome = publish_managed_component_effect(
            test_lease(&temporary).await,
            projection,
            ManagedComponentKind::Assets,
            vec![source],
        )
        .await
        .expect("zero-byte AssetObject publication");
        let ManagedComponentLifecycleOutcome::Committed(receipt) = outcome else {
            panic!("zero-byte AssetObject must commit");
        };
        let lease = receipt.into_lease();

        lease.revalidate().expect("returned Assets lease");
        assert_eq!(
            fs::metadata(
                temporary
                    .path()
                    .join("assets/objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            )
            .expect("zero-byte AssetObject")
            .len(),
            0
        );
        assert_component_lane_settled(&temporary, ManagedComponentKind::Assets);
    }

    #[tokio::test]
    async fn empty_projection_prepares_zero_shards_and_buckets() {
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let other_component = match component {
                ManagedComponentKind::Libraries => ManagedComponentKind::Assets,
                ManagedComponentKind::Assets => ManagedComponentKind::Libraries,
            };
            let (other_kind, _) = component_test_kinds(other_component);
            let other_source = test_source(
                match other_component {
                    ManagedComponentKind::Libraries => "org/example/other.jar",
                    ManagedComponentKind::Assets => "indexes/other.json",
                },
                other_kind,
                b"other-component".to_vec(),
            );
            let authority = test_authority(other_component, &[other_source]);
            let candidate = prepare_component_intent::<TestSource>(
                test_lease(&temporary).await,
                &authority,
                component,
                Vec::new(),
            )
            .await
            .expect("empty component candidate");
            let lane = temporary
                .path()
                .join(".axial-publication")
                .join(component_root_name(component));

            assert_eq!(fs::read_dir(lane.join("table")).unwrap().count(), 0);
            assert_eq!(fs::read_dir(lane.join("staging")).unwrap().count(), 0);
            assert_eq!(fs::read_dir(lane.join("quarantine")).unwrap().count(), 0);
            let prepared = candidate
                .publish_intent()
                .unwrap_or_else(|_| panic!("empty durable intent"));
            assert!(lane.join(COMPONENT_INTENT_FILE).is_file());
            let other = component_root_name(other_component);
            assert!(
                !temporary
                    .path()
                    .join(".axial-publication")
                    .join(other)
                    .exists()
            );
            drop(prepared);
        }
    }

    #[tokio::test]
    async fn duplicate_foreign_and_mismatched_sources_are_preeffect() {
        enum Mutation {
            Missing,
            Extra,
            Duplicate,
            PortableAlias,
            Kind,
            Size,
            Sha1,
        }
        for component in [
            ManagedComponentKind::Libraries,
            ManagedComponentKind::Assets,
        ] {
            for mutation in [
                Mutation::Missing,
                Mutation::Extra,
                Mutation::Duplicate,
                Mutation::PortableAlias,
                Mutation::Kind,
                Mutation::Size,
                Mutation::Sha1,
            ] {
                let temporary = tempfile::tempdir().expect("test root");
                let (first_kind, second_kind) = component_test_kinds(component);
                let paths = match component {
                    ManagedComponentKind::Libraries => [
                        "org/example/a.jar",
                        "org/example/b.jar",
                        "org/example/c.jar",
                    ],
                    ManagedComponentKind::Assets => {
                        ["indexes/a.json", "objects/ab/ab01", "objects/cd/cd02"]
                    }
                };
                let expected = vec![
                    test_source(paths[0], first_kind, b"source-a".to_vec()),
                    test_source(paths[1], second_kind, b"source-b".to_vec()),
                ];
                let authority = test_authority(component, &expected);
                let mut sources = expected;
                match mutation {
                    Mutation::Missing => {
                        sources.pop();
                    }
                    Mutation::Extra => {
                        sources.push(test_source(paths[2], second_kind, b"source-c".to_vec()))
                    }
                    Mutation::Duplicate => {
                        sources[1] = sources[0].clone();
                    }
                    Mutation::PortableAlias => {
                        let alias = match component {
                            ManagedComponentKind::Libraries => "Org/example/a.jar",
                            ManagedComponentKind::Assets => "Indexes/a.json",
                        };
                        sources[0].identity.relative_path =
                            PortableRelativePath::new(alias).expect("portable alias source path");
                    }
                    Mutation::Kind => {
                        sources[0].identity.kind = match component {
                            ManagedComponentKind::Libraries => {
                                ManagedComponentArtifactKind::AssetIndex
                            }
                            ManagedComponentKind::Assets => ManagedComponentArtifactKind::Library,
                        };
                    }
                    Mutation::Size => {
                        sources[0].identity.size += 1;
                    }
                    Mutation::Sha1 => {
                        sources[0].identity.sha1[0] ^= 0xff;
                    }
                }

                let error = match prepare_component_intent(
                    test_lease(&temporary).await,
                    &authority,
                    component,
                    sources,
                )
                .await
                {
                    Err(error) => error,
                    Ok(_) => panic!("source bijection mismatch must fail"),
                };
                assert!(matches!(error, PrepareComponentIntentError::SourceSet));
                assert_no_component_lanes(&temporary);
            }
        }
    }

    #[tokio::test]
    async fn cross_root_and_component_kind_inputs_are_rejected_before_effects() {
        for (component, wrong_root, source) in [
            (
                ManagedComponentKind::Libraries,
                KnownGoodRoot::Assets,
                test_source(
                    "org/example/wrong.jar",
                    ManagedComponentArtifactKind::Library,
                    b"wrong-library-root".to_vec(),
                ),
            ),
            (
                ManagedComponentKind::Assets,
                KnownGoodRoot::Libraries,
                test_source(
                    "objects/ab/ab01",
                    ManagedComponentArtifactKind::AssetObject,
                    b"wrong-asset-root".to_vec(),
                ),
            ),
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let known_good_kind = match source.identity.kind {
                ManagedComponentArtifactKind::Library => KnownGoodArtifactKind::Library,
                ManagedComponentArtifactKind::NativeLibrary => KnownGoodArtifactKind::NativeLibrary,
                ManagedComponentArtifactKind::AssetIndex => KnownGoodArtifactKind::AssetIndex,
                ManagedComponentArtifactKind::AssetObject => KnownGoodArtifactKind::AssetObject,
            };
            let authority = PendingKnownGoodInstallAuthority::component_for_test([(
                wrong_root,
                source.identity.relative_path.as_str().to_string(),
                known_good_kind,
                source.identity.sha1,
                source.identity.size,
            )]);

            let error = match prepare_component_intent(
                test_lease(&temporary).await,
                &authority,
                component,
                vec![source],
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("cross-root component input must fail"),
            };

            assert!(matches!(error, PrepareComponentIntentError::Projection));
            assert_no_component_lanes(&temporary);
        }

        for (component, source) in [
            (
                ManagedComponentKind::Libraries,
                test_source(
                    "objects/ab/ab01",
                    ManagedComponentArtifactKind::AssetObject,
                    b"asset-as-library".to_vec(),
                ),
            ),
            (
                ManagedComponentKind::Assets,
                test_source(
                    "org/example/library.jar",
                    ManagedComponentArtifactKind::Library,
                    b"library-as-asset".to_vec(),
                ),
            ),
        ] {
            let temporary = tempfile::tempdir().expect("test root");
            let other = match component {
                ManagedComponentKind::Libraries => ManagedComponentKind::Assets,
                ManagedComponentKind::Assets => ManagedComponentKind::Libraries,
            };
            let authority = test_authority(other, std::slice::from_ref(&source));
            let error = match prepare_component_intent(
                test_lease(&temporary).await,
                &authority,
                component,
                vec![source],
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("cross-component source kind must fail"),
            };

            assert!(matches!(error, PrepareComponentIntentError::SourceSet));
            assert_no_component_lanes(&temporary);
        }
    }
}
