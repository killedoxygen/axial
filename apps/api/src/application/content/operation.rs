use super::pack::{ModpackInstallRequest, execute_modpack_install};
use super::resolve::resolve_for_execution;
use super::{
    ContentExecutionError, ContentExecutionFailureKind, ContentInstallRequest, PlanConflict,
    conflicts_error, json_error,
};
use crate::state::{AppState, ProducerLease};
use axial_content::{
    CanonicalId, ManagedContentOperationProjection, ManagedContentPayloadSource,
    ResolutionTarget, decode_observed_content_manifest, derive_live_managed_content,
    managed_content_liveness_paths, managed_install_observation_paths,
    managed_uninstall_observation_paths, missing_managed_content_observations,
    plan_managed_content_install, plan_managed_content_uninstall,
};
use axial_minecraft::download::{
    ExecutionDownloadFact, ExecutionDownloadFactKind, PinnedTransferOrigin, RetryPolicy,
    TransferClient, TransferClientConfig, TransferFailureKind, TransferFailureReport,
    TransferOrigin, transfer_cancellation_channel,
};
use axial_minecraft::managed_path::{
    ManagedContentPlanningSession, ManagedContentPreparationOutcome, ManagedContentStageOutcome,
    ManagedContentTransactionOutcome, ManagedContentTransactionRoot, ManagedContentTransferAdvance,
    ManagedContentTransferBatch, ManagedContentTransferSettlement, ManagedContentTransferStep,
};
use axial_minecraft::DownloadProgress;
use axum::http::StatusCode;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const CONTENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
];
const CONTENT_DNS_TIMEOUT: Duration = Duration::from_secs(15);
const CONTENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONTENT_IDLE_READ_TIMEOUT: Duration = Duration::from_secs(90);
const CONTENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CONTENT_RECOVERY_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(25),
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_secs(1),
];
const MAX_CONTENT_PINNED_ADDRESSES: usize = 32;

struct ContentOperationCancellationShared {
    cancelled: AtomicBool,
    changed: Notify,
}

#[derive(Clone)]
pub(crate) struct ContentOperationCancellationSender {
    shared: Arc<ContentOperationCancellationShared>,
}

struct ContentOperationCancellation {
    shared: Arc<ContentOperationCancellationShared>,
}

/// Joined owner for one complete content workflow. Dropping the waiter requests
/// cancellation, while its producer lease keeps the worker owned through exit.
#[must_use = "content operation tasks must be joined before terminal progress"]
pub(crate) struct ContentOperationTask {
    cancellation: ContentOperationCancellationSender,
    task: Option<JoinHandle<Result<(), ContentExecutionError>>>,
}

impl ContentOperationTask {
    fn spawn<F, Fut>(producer: ProducerLease, operation: F) -> Self
    where
        F: FnOnce(ContentOperationCancellation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), ContentExecutionError>> + Send + 'static,
    {
        let shared = Arc::new(ContentOperationCancellationShared {
            cancelled: AtomicBool::new(false),
            changed: Notify::new(),
        });
        let cancellation = ContentOperationCancellationSender {
            shared: Arc::clone(&shared),
        };
        let task = producer.spawn_joinable(operation(ContentOperationCancellation { shared }));
        Self {
            cancellation,
            task: Some(task),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn cancellation_sender(&self) -> ContentOperationCancellationSender {
        self.cancellation.clone()
    }

    pub(crate) async fn join(mut self) -> Result<(), ContentExecutionError> {
        self.join_inner().await
    }

    async fn join_inner(&mut self) -> Result<(), ContentExecutionError> {
        let task = self
            .task
            .take()
            .expect("content operation retains its join handle until settlement");
        task.await.map_err(|_| operation_worker_stopped())?
    }
}

impl Drop for ContentOperationTask {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.cancel();
        }
    }
}

impl ContentOperationCancellationSender {
    pub(crate) fn cancel(&self) {
        if !self.shared.cancelled.swap(true, Ordering::AcqRel) {
            self.shared.changed.notify_one();
        }
    }
}

impl ContentOperationCancellation {
    fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            if self.is_cancelled() {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) fn start_content_install_task<F, G>(
    producer: ProducerLease,
    state: AppState,
    request: ContentInstallRequest,
    on_progress: F,
    on_download_fact: G,
) -> ContentOperationTask
where
    F: FnMut(DownloadProgress) + Send + 'static,
    G: FnMut(ExecutionDownloadFact) + Send + 'static,
{
    ContentOperationTask::spawn(producer, move |cancellation| async move {
        execute_content_install(
            state,
            request,
            cancellation,
            on_progress,
            on_download_fact,
        )
        .await
    })
}

pub(crate) fn start_content_uninstall_task<F>(
    producer: ProducerLease,
    state: AppState,
    instance_id: String,
    canonical_ids: Vec<String>,
    on_progress: F,
) -> ContentOperationTask
where
    F: FnMut(DownloadProgress) + Send + 'static,
{
    ContentOperationTask::spawn(producer, move |cancellation| async move {
        execute_content_uninstall(
            state,
            instance_id,
            canonical_ids,
            cancellation,
            on_progress,
        )
        .await
    })
}

pub(crate) fn start_modpack_install_task<F, G>(
    producer: ProducerLease,
    state: AppState,
    request: ModpackInstallRequest,
    on_progress: F,
    on_download_fact: G,
) -> ContentOperationTask
where
    F: FnMut(DownloadProgress) + Send + 'static,
    G: FnMut(ExecutionDownloadFact) + Send + 'static,
{
    ContentOperationTask::spawn(producer, move |_cancellation| async move {
        execute_modpack_install(&state, request, on_progress, on_download_fact)
            .await
            .map(|_| ())
    })
}

async fn execute_content_install<F, G>(
    state: AppState,
    request: ContentInstallRequest,
    cancellation: ContentOperationCancellation,
    mut on_progress: F,
    mut on_download_fact: G,
) -> Result<(), ContentExecutionError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    on_progress(content_progress("planning", 0, 1));
    let (target, root) = activate_content_mutation(&state, &request.instance_id).await?;
    if cancellation.is_cancelled() {
        drop(root);
        return Err(operation_cancelled());
    }

    let planning = observe_manifest(root).await?;
    let observed_manifest =
        decode_observed_content_manifest(&planning).map_err(super::content_execution_error)?;
    let liveness_paths = managed_content_liveness_paths(&observed_manifest)
        .map_err(super::content_execution_error)?;
    let planning = observe_more_if_needed(planning, liveness_paths).await?;
    let live_content = derive_live_managed_content(&observed_manifest, &planning)
        .map_err(super::content_execution_error)?;
    let resolution = resolve_for_execution(
        &state,
        &target,
        &request.selections,
        &live_content,
    )
    .await?;
    let has_unavailable = resolution
        .conflicts
        .iter()
        .any(|conflict| conflict.kind() == axial_content::ResolutionConflictKind::Unavailable);
    if has_unavailable || (!request.allow_incompatible && !resolution.conflicts.is_empty()) {
        let conflicts = resolution
            .conflicts
            .iter()
            .cloned()
            .map(PlanConflict::from)
            .collect::<Vec<_>>();
        return Err(conflicts_error(&conflicts).into());
    }
    let planned = resolution
        .to_install()
        .map_err(super::content_execution_error)?;
    if planned.is_empty() {
        return Ok(());
    }
    if cancellation.is_cancelled() {
        return Err(operation_cancelled());
    }

    let install_paths = managed_install_observation_paths(&observed_manifest, &planned)
        .map_err(super::content_execution_error)?;
    let missing = missing_managed_content_observations(&planning, install_paths)
        .map_err(super::content_execution_error)?;
    let planning = observe_more_if_needed(planning, missing).await?;
    let projection = plan_managed_content_install(&planning, observed_manifest, &planned)
        .map_err(super::content_execution_error)?;
    execute_projection(
        planning,
        projection,
        "commit",
        cancellation,
        &mut on_progress,
        &mut on_download_fact,
    )
    .await
}

async fn execute_content_uninstall<F>(
    state: AppState,
    instance_id: String,
    canonical_ids: Vec<String>,
    cancellation: ContentOperationCancellation,
    mut on_progress: F,
) -> Result<(), ContentExecutionError>
where
    F: FnMut(DownloadProgress),
{
    on_progress(content_progress("planning", 0, 1));
    let (_target, root) = activate_content_mutation(&state, &instance_id).await?;
    if cancellation.is_cancelled() {
        drop(root);
        return Err(operation_cancelled());
    }
    let planning = observe_manifest(root).await?;
    let observed_manifest =
        decode_observed_content_manifest(&planning).map_err(super::content_execution_error)?;
    let canonical_ids = canonical_ids.into_iter().map(CanonicalId).collect::<Vec<_>>();
    let uninstall_paths = managed_uninstall_observation_paths(&observed_manifest, &canonical_ids)
        .map_err(super::content_execution_error)?;
    let missing = missing_managed_content_observations(&planning, uninstall_paths)
        .map_err(super::content_execution_error)?;
    let planning = observe_more_if_needed(planning, missing).await?;
    let Some(projection) = plan_managed_content_uninstall(
        &planning,
        observed_manifest,
        &canonical_ids,
    )
    .map_err(super::content_execution_error)?
    else {
        return Ok(());
    };
    let mut ignore_download_fact = |_: ExecutionDownloadFact| {};
    execute_projection(
        planning,
        projection,
        "removing",
        cancellation,
        &mut on_progress,
        &mut ignore_download_fact,
    )
    .await
}

async fn activate_content_mutation(
    state: &AppState,
    instance_id: &str,
) -> Result<(ResolutionTarget, ManagedContentTransactionRoot), ContentExecutionError> {
    let lifecycle = state
        .try_acquire_instance_lifecycle(instance_id)
        .await
        .ok_or_else(operation_busy)?;
    let admission = state
        .admit_instance_content_mutation(lifecycle)
        .await
        .map_err(content_authority_error)?;
    let activated = run_blocking(move || admission.activate())
        .await?
        .map_err(content_authority_error)?;
    let (identity, root) = activated.into_parts();
    Ok((
        ResolutionTarget {
            loader: identity.loader_key().to_string(),
            game_version: identity.minecraft_version().to_string(),
            supports_mods: identity.supports_mods(),
        },
        root,
    ))
}

async fn observe_manifest(
    root: ManagedContentTransactionRoot,
) -> Result<ManagedContentPlanningSession, ContentExecutionError> {
    run_blocking(move || root.observe_manifest())
        .await?
        .map_err(|_| content_filesystem_failed())
}

async fn observe_more_if_needed(
    planning: ManagedContentPlanningSession,
    paths: Vec<axial_minecraft::portable_path::PortableRelativePath>,
) -> Result<ManagedContentPlanningSession, ContentExecutionError> {
    if paths.is_empty() {
        return Ok(planning);
    }
    run_blocking(move || planning.observe_more(paths))
        .await?
        .map_err(|_| content_filesystem_failed())
}

async fn execute_projection<F, G>(
    planning: ManagedContentPlanningSession,
    projection: ManagedContentOperationProjection,
    commit_phase: &'static str,
    cancellation: ContentOperationCancellation,
    on_progress: &mut F,
    on_download_fact: &mut G,
) -> Result<(), ContentExecutionError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    let effect_paths = projection.effect_paths();
    let session = run_blocking(move || planning.finish(effect_paths))
        .await?
        .map_err(|_| content_filesystem_failed())?;
    let execution = projection
        .seal(&session)
        .map_err(super::content_execution_error)?;
    let (mutation, sources, affected_entries) = execution.into_parts();
    let preparation = run_blocking(move || session.prepare(mutation)).await?;
    let prepared = match preparation {
        ManagedContentPreparationOutcome::Prepared(prepared) => prepared,
        ManagedContentPreparationOutcome::Refused { .. } => {
            return Err(content_filesystem_failed());
        }
        ManagedContentPreparationOutcome::RecoveryRequired(recovery) => {
            return settle_transaction_outcome(
                ManagedContentTransactionOutcome::RecoveryRequired(recovery),
                false,
            )
            .await;
        }
    };
    if cancellation.is_cancelled() {
        let outcome = run_blocking(move || prepared.cancel()).await?;
        return settle_transaction_outcome(outcome, true).await;
    }

    let transfers = prepared.into_transfer_batch();
    let payload_count = transfers.payload_count();
    let total = i32::try_from(payload_count).unwrap_or(i32::MAX);
    on_progress(content_progress(
        if payload_count == 0 {
            commit_phase
        } else {
            "download"
        },
        0,
        total.max(1),
    ));
    let outcome = execute_transfers(
        transfers,
        sources,
        commit_phase,
        affected_entries,
        &cancellation,
        on_progress,
        on_download_fact,
    )
    .await?;
    let settled = settle_transaction_outcome(outcome, cancellation.is_cancelled()).await;
    if settled.is_ok() {
        for _ in 0..payload_count {
            on_download_fact(download_fact(ExecutionDownloadFactKind::Promoted));
        }
    }
    settled
}

async fn execute_transfers<F, G>(
    mut transfers: ManagedContentTransferBatch,
    sources: Vec<ManagedContentPayloadSource>,
    commit_phase: &'static str,
    affected_entries: usize,
    cancellation: &ContentOperationCancellation,
    on_progress: &mut F,
    on_download_fact: &mut G,
) -> Result<ManagedContentTransactionOutcome, ContentExecutionError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    let mut sources = sources
        .into_iter()
        .map(|source| {
            let (id, url, display_name) = source.into_parts();
            (id, (url, Some(display_name)))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let payload_count = transfers.payload_count();
    let total = i32::try_from(payload_count).unwrap_or(i32::MAX).max(1);
    let mut completed = 0_usize;
    let retry = RetryPolicy::classified(&CONTENT_RETRY_DELAYS, content_transfer_retryable)
        .expect("fixed content retry policy is valid");

    loop {
        if cancellation.is_cancelled() {
            let outcome = run_blocking(move || transfers.cancel()).await?;
            return settle_transfer_abort(outcome, operation_cancelled()).await;
        }
        match transfers.next() {
            ManagedContentTransferStep::Issued(issued) => {
                let Some((url, display_name)) = sources.remove(issued.id()) else {
                    let outcome = run_blocking(move || issued.cancel()).await?;
                    return settle_transfer_abort(outcome, content_provider_metadata_failed()).await;
                };
                on_progress(content_file_progress(
                    "download",
                    i32::try_from(completed).unwrap_or(i32::MAX),
                    total,
                    display_name,
                ));
                let client = match pinned_transfer_client(&url, cancellation).await {
                    Ok(client) => client,
                    Err(error) => {
                        let outcome = run_blocking(move || issued.cancel()).await?;
                        return settle_transfer_abort(outcome, error).await;
                    }
                };
                let (transfer_cancellation, transfer_cancelled) =
                    transfer_cancellation_channel();
                let transfer = issued.start(
                    client,
                    url,
                    retry.clone(),
                    transfer_cancelled,
                );
                let joined = transfer.join();
                tokio::pin!(joined);
                let settlement = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        transfer_cancellation.cancel();
                        joined.await
                    }
                    settlement = &mut joined => settlement,
                };
                drop(transfer_cancellation);
                record_transfer_settlement(&settlement, on_download_fact);
                let failure = settlement
                    .failure_report()
                    .map(|report| transfer_failure_error(report, cancellation.is_cancelled()));
                match run_blocking(move || settlement.advance()).await? {
                    ManagedContentTransferAdvance::Continue(next) => {
                        transfers = next;
                        completed += 1;
                    }
                    ManagedContentTransferAdvance::Unwind(outcome) => {
                        return settle_transfer_abort(
                            outcome,
                            failure.unwrap_or_else(content_download_failed),
                        )
                        .await;
                    }
                }
            }
            ManagedContentTransferStep::Complete(complete) => {
                if !sources.is_empty() {
                    let outcome = run_blocking(move || complete.cancel()).await?;
                    return settle_transfer_abort(
                        outcome,
                        content_provider_metadata_failed(),
                    )
                    .await;
                }
                if cancellation.is_cancelled() {
                    let outcome = run_blocking(move || complete.cancel()).await?;
                    return settle_transfer_abort(outcome, operation_cancelled()).await;
                }
                let current = i32::try_from(affected_entries).unwrap_or(i32::MAX);
                on_progress(content_progress(commit_phase, current, current.max(1)));
                let stage = run_blocking(move || complete.stage()).await?;
                return Ok(match stage {
                    ManagedContentStageOutcome::Ready(ready) => {
                        if cancellation.is_cancelled() {
                            run_blocking(move || ready.cancel()).await?
                        } else {
                            run_blocking(move || ready.commit()).await?
                        }
                    }
                    ManagedContentStageOutcome::Unwind(outcome) => outcome,
                });
            }
        }
    }
}

async fn settle_transfer_abort(
    outcome: ManagedContentTransactionOutcome,
    error: ContentExecutionError,
) -> Result<ManagedContentTransactionOutcome, ContentExecutionError> {
    match settle_transaction(outcome).await? {
        SettledContentTransaction::Cancelled => Err(error),
        SettledContentTransaction::Committed | SettledContentTransaction::Failed => {
            Err(content_filesystem_failed())
        }
    }
}

async fn pinned_transfer_client(
    url: &reqwest::Url,
    cancellation: &ContentOperationCancellation,
) -> Result<TransferClient, ContentExecutionError> {
    let origin = TransferOrigin::from_url(url).map_err(|_| content_provider_metadata_failed())?;
    let host = url
        .host_str()
        .ok_or_else(content_provider_metadata_failed)?
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(content_provider_metadata_failed)?;
    let lookup = tokio::net::lookup_host((host.as_str(), port));
    let addresses = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(operation_cancelled()),
        result = tokio::time::timeout(CONTENT_DNS_TIMEOUT, lookup) => {
            let resolved = result
                .map_err(|_| content_download_failed())?
                .map_err(|_| content_download_failed())?;
            bounded_unique_addresses(resolved)
        }
    };
    let pinned = PinnedTransferOrigin::public(origin, addresses)
        .map_err(|_| content_download_failed())?;
    let config = TransferClientConfig::bounded_pinned_public(
        CONTENT_CONNECT_TIMEOUT,
        CONTENT_IDLE_READ_TIMEOUT,
        CONTENT_REQUEST_TIMEOUT,
        vec![pinned],
    )
    .map_err(|_| content_download_failed())?;
    TransferClient::build(config).map_err(|_| content_download_failed())
}

fn bounded_unique_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Vec<SocketAddr> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(MAX_CONTENT_PINNED_ADDRESSES);
    for address in addresses {
        if seen.insert(address) {
            if unique.len() == MAX_CONTENT_PINNED_ADDRESSES {
                break;
            }
            unique.push(address);
        }
    }
    unique
}

fn content_transfer_retryable(failure: &TransferFailureKind) -> bool {
    matches!(
        failure,
        TransferFailureKind::Network
            | TransferFailureKind::ProviderStatus(408 | 429 | 500..=599)
    )
}

fn record_transfer_settlement<G>(
    settlement: &ManagedContentTransferSettlement,
    on_download_fact: &mut G,
) where
    G: FnMut(ExecutionDownloadFact),
{
    if settlement.is_complete() {
        on_download_fact(download_fact(ExecutionDownloadFactKind::WrittenToTemp));
    } else if let Some(report) = settlement.failure_report() {
        record_transfer_failure(report, on_download_fact);
    }
}

fn transfer_failure_error(
    report: &TransferFailureReport,
    cancellation_requested: bool,
) -> ContentExecutionError {
    match report.last() {
        TransferFailureKind::Cancelled if cancellation_requested => operation_cancelled(),
        TransferFailureKind::Network => content_download_failed(),
        TransferFailureKind::ProviderStatus(_)
        | TransferFailureKind::RequestPolicy
        | TransferFailureKind::ContentEncodingRejected
        | TransferFailureKind::ContentLengthContractMismatch { .. }
        | TransferFailureKind::ContentLengthMismatch { .. }
        | TransferFailureKind::SizeMismatch { .. }
        | TransferFailureKind::ByteLimitExceeded { .. }
        | TransferFailureKind::ByteCountOverflow
        | TransferFailureKind::DigestMismatch(_) => content_provider_failed(),
        TransferFailureKind::StageCreate(kind)
        | TransferFailureKind::StageWrite(kind)
        | TransferFailureKind::StageSeal(kind)
            if kind == std::io::ErrorKind::PermissionDenied =>
        {
            content_permission_failed()
        }
        TransferFailureKind::Cancelled
        | TransferFailureKind::StageCreate(_)
        | TransferFailureKind::StageWrite(_)
        | TransferFailureKind::StageSeal(_)
        | TransferFailureKind::ChannelClosed
        | TransferFailureKind::ProducerWorkerMismatch { .. }
        | TransferFailureKind::WorkerStopped => content_filesystem_failed(),
    }
}

fn record_transfer_failure<G>(report: &TransferFailureReport, on_download_fact: &mut G)
where
    G: FnMut(ExecutionDownloadFact),
{
    for event in report.events() {
        let kind = match event.kind() {
            TransferFailureKind::Cancelled => ExecutionDownloadFactKind::Interrupted,
            TransferFailureKind::Network => ExecutionDownloadFactKind::NetworkFailure,
            TransferFailureKind::ProviderStatus(_)
            | TransferFailureKind::RequestPolicy
            | TransferFailureKind::ContentEncodingRejected => {
                ExecutionDownloadFactKind::ProviderFailure
            }
            TransferFailureKind::ContentLengthContractMismatch { .. }
            | TransferFailureKind::ContentLengthMismatch { .. }
            | TransferFailureKind::SizeMismatch { .. }
            | TransferFailureKind::ByteLimitExceeded { .. }
            | TransferFailureKind::ByteCountOverflow => ExecutionDownloadFactKind::SizeMismatch,
            TransferFailureKind::DigestMismatch(_) => ExecutionDownloadFactKind::ChecksumMismatch,
            TransferFailureKind::StageCreate(kind)
            | TransferFailureKind::StageWrite(kind)
            | TransferFailureKind::StageSeal(kind)
                if kind == std::io::ErrorKind::PermissionDenied => {
                    ExecutionDownloadFactKind::PermissionFailure
                }
            TransferFailureKind::StageCreate(_)
            | TransferFailureKind::StageWrite(_)
            | TransferFailureKind::StageSeal(_)
            | TransferFailureKind::ChannelClosed
            | TransferFailureKind::ProducerWorkerMismatch { .. }
            | TransferFailureKind::WorkerStopped => ExecutionDownloadFactKind::TempWriteFailed,
        };
        on_download_fact(download_fact(kind));
    }
}

fn download_fact(kind: ExecutionDownloadFactKind) -> ExecutionDownloadFact {
    ExecutionDownloadFact {
        kind,
        target: "content_artifact".to_string(),
        fields: Vec::new(),
    }
}

async fn settle_transaction_outcome(
    outcome: ManagedContentTransactionOutcome,
    cancelled: bool,
) -> Result<(), ContentExecutionError> {
    match settle_transaction(outcome).await? {
        SettledContentTransaction::Committed => Ok(()),
        SettledContentTransaction::Cancelled if cancelled => Err(operation_cancelled()),
        SettledContentTransaction::Cancelled | SettledContentTransaction::Failed => {
            Err(content_filesystem_failed())
        }
    }
}

enum SettledContentTransaction {
    Committed,
    Cancelled,
    Failed,
}

async fn settle_transaction(
    mut outcome: ManagedContentTransactionOutcome,
) -> Result<SettledContentTransaction, ContentExecutionError> {
    let mut retry_index = 0;
    loop {
        outcome = match outcome {
            ManagedContentTransactionOutcome::Committed(_) => {
                return Ok(SettledContentTransaction::Committed);
            }
            ManagedContentTransactionOutcome::Cancelled(_) => {
                return Ok(SettledContentTransaction::Cancelled);
            }
            ManagedContentTransactionOutcome::Failed(_) => {
                return Ok(SettledContentTransaction::Failed);
            }
            ManagedContentTransactionOutcome::RecoveryRequired(recovery) => {
                tokio::time::sleep(CONTENT_RECOVERY_RETRY_DELAYS[retry_index]).await;
                retry_index = (retry_index + 1).min(CONTENT_RECOVERY_RETRY_DELAYS.len() - 1);
                run_blocking(move || recovery.reconcile()).await?
            }
        };
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T, ContentExecutionError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| operation_worker_stopped())
}

fn content_progress(phase: &str, current: i32, total: i32) -> DownloadProgress {
    content_file_progress(phase, current, total, None)
}

fn content_file_progress(
    phase: &str,
    current: i32,
    total: i32,
    file: Option<String>,
) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

fn operation_busy() -> ContentExecutionError {
    ContentExecutionError::from(json_error(
        StatusCode::CONFLICT,
        "another launch or content operation is already using this instance",
    ))
}

fn content_authority_error(error: std::io::Error) -> ContentExecutionError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ContentExecutionError::from(json_error(
            StatusCode::NOT_FOUND,
            "instance not found",
        )),
        std::io::ErrorKind::WouldBlock => operation_busy(),
        std::io::ErrorKind::PermissionDenied => ContentExecutionError {
            response: json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not complete the content operation",
            ),
            failure_kind: Some(ContentExecutionFailureKind::PermissionDenied),
        },
        _ => content_filesystem_failed(),
    }
}

fn content_filesystem_failed() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not complete the content operation",
        ),
        failure_kind: Some(ContentExecutionFailureKind::FileOperation),
    }
}

fn content_download_failed() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::BAD_GATEWAY,
            "a content download failed; check your connection and try again",
        ),
        failure_kind: Some(ContentExecutionFailureKind::NetworkFailure),
    }
}

fn content_provider_failed() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::BAD_GATEWAY,
            "the content provider returned an invalid artifact; try again later",
        ),
        failure_kind: Some(ContentExecutionFailureKind::ProviderFailure),
    }
}

fn content_permission_failed() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not complete the content operation",
        ),
        failure_kind: Some(ContentExecutionFailureKind::PermissionDenied),
    }
}

fn content_provider_metadata_failed() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::BAD_GATEWAY,
            "the content provider returned invalid metadata; try again later",
        ),
        failure_kind: Some(ContentExecutionFailureKind::MetadataInvalid),
    }
}

fn operation_cancelled() -> ContentExecutionError {
    ContentExecutionError::from(json_error(
        StatusCode::CONFLICT,
        "content operation stopped before completing",
    ))
}

fn operation_worker_stopped() -> ContentExecutionError {
    ContentExecutionError {
        response: json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "content operation stopped before completing",
        ),
        failure_kind: Some(ContentExecutionFailureKind::FileOperation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_retry_policy_excludes_early_data() {
        for failure in [
            TransferFailureKind::Network,
            TransferFailureKind::ProviderStatus(408),
            TransferFailureKind::ProviderStatus(429),
            TransferFailureKind::ProviderStatus(500),
            TransferFailureKind::ProviderStatus(599),
        ] {
            assert!(content_transfer_retryable(&failure));
        }
        for failure in [
            TransferFailureKind::ProviderStatus(425),
            TransferFailureKind::ProviderStatus(404),
            TransferFailureKind::ProviderStatus(600),
            TransferFailureKind::RequestPolicy,
        ] {
            assert!(!content_transfer_retryable(&failure));
        }
    }

    #[test]
    fn operation_task_is_move_only() {
        static_assertions::assert_not_impl_any!(ContentOperationTask: Clone);
    }

    #[test]
    fn dns_addresses_are_deduplicated_before_the_bound() {
        let first: SocketAddr = "203.0.113.1:443".parse().expect("first address");
        let second: SocketAddr = "203.0.113.2:443".parse().expect("second address");

        assert_eq!(
            bounded_unique_addresses([first, first, second, first]),
            vec![first, second]
        );
    }

    #[test]
    fn dns_addresses_are_capped_at_the_pinning_bound() {
        let addresses = (1_u8..=40).map(|suffix| {
            SocketAddr::from(([203, 0, 113, suffix], 443))
        });

        let bounded = bounded_unique_addresses(addresses);

        assert_eq!(bounded.len(), MAX_CONTENT_PINNED_ADDRESSES);
        assert_eq!(
            bounded.last(),
            Some(&SocketAddr::from(([203, 0, 113, 32], 443)))
        );
    }
}
