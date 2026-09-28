use super::model::InstallError;
use super::plan::{ManagedArtifactPin, ManagedCompositionInstallPlan};
use crate::state::{
    ManagedArtifactCandidateIntent, prepare_managed_artifact_addition,
    prepare_managed_artifact_candidate, publish_managed_artifact_addition,
    reconcile_managed_candidate_intents,
};
use crate::storage::{ManagedInstanceEffectAuthority, ManagedStorageDirectory, ManagedStorageFile};
use crate::types::{
    CompositionState, InstalledMod, ManagedArtifactIntegrity, ManagedArtifactProvider,
    ManagedArtifactSource, ManagedDependencyStateEdge, OwnershipClass,
};
use axial_minecraft::download::{
    CreateOnlyTransferTarget, ExpectedTransferDigests, ManagedTransferAuthority, RetryPolicy,
    TransferCleanupObligation, TransferCleanupResolution, TransferClient, TransferContract,
    TransferFailureReport, TransferOutcome, TransferPublicationObligation,
    TransferPublicationOutcome, TransferReport, TransferTargetCancelObligation,
    TransferTargetCancelOutcome, TransferUnsettledObligation, VerifiedCreateOnly,
    VerifiedTransferDiscardObligation, VerifiedTransferDiscardOutcome, start_create_only_transfer,
    transfer_cancellation_channel,
};
use chrono::Utc;
use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::sync::Arc;

const MANAGED_GRAPH_STAGE_CONCURRENCY: usize = 4;

#[derive(Clone)]
pub struct ManagedArtifactTransferResolver {
    resolve: Arc<
        dyn Fn(reqwest::Url) -> futures_util::future::BoxFuture<'static, io::Result<TransferClient>>
            + Send
            + Sync,
    >,
    retry: RetryPolicy,
}

impl fmt::Debug for ManagedArtifactTransferResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedArtifactTransferResolver")
            .finish_non_exhaustive()
    }
}

impl ManagedArtifactTransferResolver {
    pub fn new<Resolve, ResolveFuture>(resolve: Resolve, retry: RetryPolicy) -> Self
    where
        Resolve: Fn(reqwest::Url) -> ResolveFuture + Send + Sync + 'static,
        ResolveFuture: Future<Output = io::Result<TransferClient>> + Send + 'static,
    {
        Self {
            resolve: Arc::new(move |url| resolve(url).boxed()),
            retry,
        }
    }

    async fn client(&self, url: reqwest::Url) -> io::Result<TransferClient> {
        (self.resolve)(url).await
    }
}

pub(super) struct StagedManagedArtifact {
    installed: InstalledMod,
    candidate: ManagedStorageFile,
    intent: ManagedArtifactCandidateIntent,
}

pub(super) trait ManagedArtifactStage {
    fn installed(&self) -> &InstalledMod;
    fn publish_create_new(self, destination: &ManagedStorageDirectory) -> Result<(), InstallError>;
}

impl ManagedArtifactStage for StagedManagedArtifact {
    fn installed(&self) -> &InstalledMod {
        &self.installed
    }

    fn publish_create_new(self, destination: &ManagedStorageDirectory) -> Result<(), InstallError> {
        self.intent.validate()?;
        let addition = prepare_managed_artifact_addition(destination, &self.installed)?;
        destination.copy_file_create_new(
            &self.candidate,
            std::path::Path::new(&self.installed.filename),
            crate::MANAGED_ARTIFACT_MAX_BYTES,
        )?;
        publish_managed_artifact_addition(destination, &self.installed, &addition)?;
        Ok(())
    }
}

struct PreparedArtifactTransfer {
    installed: InstalledMod,
    intent: ManagedArtifactCandidateIntent,
    target: CreateOnlyTransferTarget,
    client: Option<TransferClient>,
    url: reqwest::Url,
    contract: TransferContract,
}

struct ArtifactTransferSpec {
    installed: InstalledMod,
    url: reqwest::Url,
    contract: TransferContract,
}

struct JoinedArtifactTransfer {
    installed: InstalledMod,
    intent: ManagedArtifactCandidateIntent,
    outcome: TransferOutcome<VerifiedCreateOnly>,
}

enum TransferRecoveryMember {
    TargetCancel(TransferTargetCancelObligation),
    Cleanup(TransferCleanupObligation),
    Unsettled(TransferUnsettledObligation),
    VerifiedDiscard(VerifiedTransferDiscardObligation),
    Publication(TransferPublicationObligation),
}

pub(crate) struct ManagedArtifactTransferRecovery {
    instance_mods: ManagedStorageDirectory,
    members: Vec<TransferRecoveryMember>,
}

impl ManagedArtifactTransferRecovery {
    pub(crate) fn reconcile(mut self) -> Result<Option<Self>, (io::Error, Self)> {
        let mut pending = Vec::with_capacity(self.members.len());
        for member in self.members.drain(..) {
            match member {
                TransferRecoveryMember::TargetCancel(obligation) => {
                    if let TransferTargetCancelOutcome::Pending(obligation) = obligation.reconcile()
                    {
                        pending.push(TransferRecoveryMember::TargetCancel(obligation));
                    }
                }
                TransferRecoveryMember::Cleanup(obligation) => {
                    if let TransferCleanupResolution::Pending(obligation) = obligation.reconcile() {
                        pending.push(TransferRecoveryMember::Cleanup(obligation));
                    }
                }
                TransferRecoveryMember::Unsettled(obligation) => {
                    if let Err(obligation) = obligation.reconcile_retained_effects() {
                        pending.push(TransferRecoveryMember::Unsettled(obligation));
                    }
                }
                TransferRecoveryMember::VerifiedDiscard(obligation) => {
                    if let VerifiedTransferDiscardOutcome::Pending(obligation) =
                        obligation.reconcile()
                    {
                        pending.push(TransferRecoveryMember::VerifiedDiscard(obligation));
                    }
                }
                TransferRecoveryMember::Publication(obligation) => {
                    reconcile_publication_recovery(obligation.reconcile(), &mut pending);
                }
            }
        }
        self.members = pending;
        if !self.members.is_empty() {
            return Ok(Some(self));
        }
        if let Err(error) = reconcile_managed_candidate_intents(&self.instance_mods) {
            return Err((io::Error::other(error), self));
        }
        Ok(None)
    }
}

pub(super) async fn stage_managed_graph(
    resolver: ManagedArtifactTransferResolver,
    pins: Vec<ManagedArtifactPin>,
    instance_mods: &ManagedStorageDirectory,
) -> Result<Vec<StagedManagedArtifact>, InstallError> {
    if pins.is_empty() {
        return Ok(Vec::new());
    }
    let effect_authority = Arc::new(instance_mods.effect_owner().clone());
    let root = instance_mods.clone();
    let prepared =
        super::mutation::run_managed_blocking(axial_resource::PhysicalIoClass::Write, move || {
            prepare_transfer_targets(&root, pins, effect_authority)
        })
        .await
        .map_err(|_| InstallError::Io(io::Error::other("managed candidate preparation stopped")))?;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err((error, prepared)) => {
            return abort_prepared_transfers(instance_mods, prepared.into(), Vec::new(), error)
                .await;
        }
    };
    let mut prepared = VecDeque::from(prepared);
    let mut resolved = VecDeque::with_capacity(prepared.len());
    while let Some(mut target) = prepared.pop_front() {
        match resolver.client(target.url.clone()).await {
            Ok(client) if client.admits_url(&target.url) => {
                target.client = Some(client);
                resolved.push_back(target);
            }
            Ok(_) | Err(_) => {
                prepared.push_front(target);
                prepared.extend(resolved);
                return abort_prepared_transfers(
                    instance_mods,
                    prepared,
                    Vec::new(),
                    InstallError::Transfer,
                )
                .await;
            }
        }
    }

    let concurrency = resolved.len().clamp(1, MANAGED_GRAPH_STAGE_CONCURRENCY);
    let mut active = FuturesUnordered::new();
    let mut cancellations = Vec::with_capacity(resolved.len());
    let mut complete = Vec::with_capacity(resolved.len());
    for _ in 0..concurrency {
        start_next_transfer(
            &mut resolved,
            &mut active,
            &mut cancellations,
            &resolver.retry,
        );
    }
    let mut failed = false;
    let mut recovery = Vec::new();
    while let Some(joined) = active.next().await {
        let transfer_failed = !matches!(joined.outcome, TransferOutcome::Complete(_));
        if let Some(report) = transfer_failure_report(&joined.outcome) {
            tracing::warn!(
                failure_kind = ?report.last(),
                attempt_count = report.attempts(),
                "managed performance artifact transfer failed"
            );
        }
        if transfer_failed && !failed {
            failed = true;
            for cancellation in &cancellations {
                cancellation.cancel();
            }
        }
        if failed {
            retain_transfer_outcome(joined.outcome, &mut recovery);
        } else {
            complete.push((joined.installed, joined.intent, joined.outcome));
            start_next_transfer(
                &mut resolved,
                &mut active,
                &mut cancellations,
                &resolver.retry,
            );
        }
    }
    if failed {
        for (_, _, outcome) in complete {
            retain_transfer_outcome(outcome, &mut recovery);
        }
        return abort_prepared_transfers(instance_mods, resolved, recovery, InstallError::Transfer)
            .await;
    }

    let root = instance_mods.clone();
    super::mutation::run_managed_blocking(axial_resource::PhysicalIoClass::Heavy, move || {
        publish_transfer_candidates(&root, complete)
    })
    .await
    .map_err(|_| InstallError::Io(io::Error::other("managed candidate publication stopped")))?
}

fn transfer_failure_report<T>(outcome: &TransferOutcome<T>) -> Option<&TransferFailureReport> {
    match outcome {
        TransferOutcome::Complete(_) => None,
        TransferOutcome::Failed { report, .. } => Some(report),
        TransferOutcome::CleanupPending(obligation) => Some(obligation.report()),
        TransferOutcome::Unsettled(obligation) => Some(obligation.report()),
    }
}

fn prepare_transfer_targets(
    instance_mods: &ManagedStorageDirectory,
    pins: Vec<ManagedArtifactPin>,
    effect_authority: Arc<ManagedInstanceEffectAuthority>,
) -> Result<Vec<PreparedArtifactTransfer>, (InstallError, Vec<PreparedArtifactTransfer>)> {
    let specs = pins
        .into_iter()
        .map(|pin| {
            let installed = installed_from_pin(&pin);
            let url =
                reqwest::Url::parse(pin.download_url()).map_err(|_| invalid_transfer_plan())?;
            let contract = TransferContract::authenticated_exact(
                NonZeroU64::new(pin.size()).ok_or_else(invalid_transfer_plan)?,
                ExpectedTransferDigests::from_hex(None, Some(pin.sha512()))
                    .map_err(|_| invalid_transfer_plan())?,
            )
            .map_err(|_| invalid_transfer_plan())?;
            Ok(ArtifactTransferSpec {
                installed,
                url,
                contract,
            })
        })
        .collect::<Result<Vec<_>, InstallError>>()
        .map_err(|error| (error, Vec::new()))?;
    let mut prepared = Vec::with_capacity(specs.len());
    for spec in specs {
        let intent = match prepare_managed_artifact_candidate(instance_mods, &spec.installed) {
            Ok(intent) => intent,
            Err(error) => return Err((error.into(), prepared)),
        };
        let destination = match intent
            .candidate_parent()
            .admit_transient_destination(intent.candidate_name())
        {
            Ok(destination) => destination,
            Err(error) => return Err((error.into(), prepared)),
        };
        prepared.push(PreparedArtifactTransfer {
            installed: spec.installed,
            intent,
            target: CreateOnlyTransferTarget::new(
                destination,
                ManagedTransferAuthority::retain_with_effect_settlement(Arc::clone(
                    &effect_authority,
                )),
            ),
            client: None,
            url: spec.url,
            contract: spec.contract,
        });
    }
    Ok(prepared)
}

type ActiveArtifactTransfer = futures_util::future::BoxFuture<'static, JoinedArtifactTransfer>;

fn start_next_transfer(
    prepared: &mut VecDeque<PreparedArtifactTransfer>,
    active: &mut FuturesUnordered<ActiveArtifactTransfer>,
    cancellations: &mut Vec<axial_minecraft::download::TransferCancellationSender>,
    retry: &RetryPolicy,
) {
    let Some(prepared) = prepared.pop_front() else {
        return;
    };
    let PreparedArtifactTransfer {
        installed,
        intent,
        target,
        client,
        url,
        contract,
    } = prepared;
    let client = client.expect("resolved managed artifact retains its exact client");
    let (cancellation_sender, cancellation) = transfer_cancellation_channel();
    cancellations.push(cancellation_sender);
    let task =
        start_create_only_transfer(client, url, target, contract, retry.clone(), cancellation);
    active.push(
        async move {
            JoinedArtifactTransfer {
                installed,
                intent,
                outcome: task.join().await,
            }
        }
        .boxed(),
    );
}

async fn abort_prepared_transfers(
    instance_mods: &ManagedStorageDirectory,
    prepared: VecDeque<PreparedArtifactTransfer>,
    mut members: Vec<TransferRecoveryMember>,
    original: InstallError,
) -> Result<Vec<StagedManagedArtifact>, InstallError> {
    let root = instance_mods.clone();
    super::mutation::run_managed_blocking(axial_resource::PhysicalIoClass::Heavy, move || {
        for prepared in prepared {
            if let TransferTargetCancelOutcome::Pending(obligation) = prepared.target.cancel() {
                members.push(TransferRecoveryMember::TargetCancel(obligation));
            }
        }
        let effects = root.effect_owner().clone();
        let recovery = ManagedArtifactTransferRecovery {
            instance_mods: root,
            members,
        };
        match recovery.reconcile() {
            Ok(None) => original,
            Ok(Some(recovery)) => InstallError::Io(effects.retain_artifact_transfer(recovery)),
            Err((error, recovery)) => {
                let _pending = effects.retain_artifact_transfer(recovery);
                InstallError::Io(error)
            }
        }
    })
    .await
    .map(Err)
    .unwrap_or_else(|_| {
        Err(InstallError::Io(io::Error::other(
            "managed artifact transfer cleanup stopped",
        )))
    })
}

fn retain_transfer_outcome(
    outcome: TransferOutcome<VerifiedCreateOnly>,
    members: &mut Vec<TransferRecoveryMember>,
) {
    match outcome {
        TransferOutcome::Complete(verified) => retain_verified_discard(verified, members),
        TransferOutcome::Failed { .. } => {}
        TransferOutcome::CleanupPending(obligation) => {
            members.push(TransferRecoveryMember::Cleanup(obligation));
        }
        TransferOutcome::Unsettled(obligation) => {
            members.push(TransferRecoveryMember::Unsettled(obligation));
        }
    }
}

fn retain_verified_discard(
    verified: VerifiedCreateOnly,
    members: &mut Vec<TransferRecoveryMember>,
) {
    if let VerifiedTransferDiscardOutcome::Pending(obligation) = verified.discard() {
        members.push(TransferRecoveryMember::VerifiedDiscard(obligation));
    }
}

fn reconcile_publication_recovery(
    outcome: TransferPublicationOutcome,
    pending: &mut Vec<TransferRecoveryMember>,
) {
    match outcome {
        TransferPublicationOutcome::Published { file, .. } => drop(file),
        TransferPublicationOutcome::NoEffect { verified, .. } => {
            retain_verified_discard(verified, pending);
        }
        TransferPublicationOutcome::Pending(obligation) => {
            pending.push(TransferRecoveryMember::Publication(obligation));
        }
    }
}

fn publish_transfer_candidates(
    instance_mods: &ManagedStorageDirectory,
    complete: Vec<(
        InstalledMod,
        ManagedArtifactCandidateIntent,
        TransferOutcome<VerifiedCreateOnly>,
    )>,
) -> Result<Vec<StagedManagedArtifact>, InstallError> {
    let mut staged = Vec::with_capacity(complete.len());
    let mut remaining = complete.into_iter();
    while let Some((installed, intent, outcome)) = remaining.next() {
        let verified = match outcome {
            TransferOutcome::Complete(verified) => verified,
            outcome => {
                let mut recovery = Vec::new();
                retain_transfer_outcome(outcome, &mut recovery);
                for (_, _, outcome) in remaining {
                    retain_transfer_outcome(outcome, &mut recovery);
                }
                return retain_publication_recovery(
                    instance_mods,
                    recovery,
                    InstallError::Transfer,
                );
            }
        };
        let report = verified.report().clone();
        match verified.publish_create_new() {
            TransferPublicationOutcome::Published {
                file,
                report: published_report,
                authority: _,
            } => {
                if report != published_report || !report_matches_artifact(&report, &installed) {
                    drop(file);
                    let recovery = remaining.map(|(_, _, outcome)| outcome).fold(
                        Vec::new(),
                        |mut recovery, outcome| {
                            retain_transfer_outcome(outcome, &mut recovery);
                            recovery
                        },
                    );
                    return retain_publication_recovery(
                        instance_mods,
                        recovery,
                        invalid_transfer_plan(),
                    );
                }
                let candidate = (|| -> Result<ManagedStorageFile, InstallError> {
                    intent.validate()?;
                    let candidate = intent
                        .candidate_parent()
                        .bind_transient_publication(intent.candidate_name(), file)?;
                    if candidate.size() != installed.size
                        || !candidate
                            .sha512(crate::MANAGED_ARTIFACT_MAX_BYTES)?
                            .eq_ignore_ascii_case(&installed.integrity.sha512)
                    {
                        return Err(invalid_transfer_plan());
                    }
                    Ok(candidate)
                })();
                let candidate = match candidate {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        drop(staged);
                        let recovery = remaining.map(|(_, _, outcome)| outcome).fold(
                            Vec::new(),
                            |mut recovery, outcome| {
                                retain_transfer_outcome(outcome, &mut recovery);
                                recovery
                            },
                        );
                        return retain_publication_recovery(instance_mods, recovery, error);
                    }
                };
                staged.push(StagedManagedArtifact {
                    installed,
                    candidate,
                    intent,
                });
            }
            TransferPublicationOutcome::NoEffect { verified, .. } => {
                let mut recovery = Vec::new();
                retain_verified_discard(verified, &mut recovery);
                for (_, _, outcome) in remaining {
                    retain_transfer_outcome(outcome, &mut recovery);
                }
                return retain_publication_recovery(
                    instance_mods,
                    recovery,
                    InstallError::Transfer,
                );
            }
            TransferPublicationOutcome::Pending(obligation) => {
                let mut recovery = vec![TransferRecoveryMember::Publication(obligation)];
                for (_, _, outcome) in remaining {
                    retain_transfer_outcome(outcome, &mut recovery);
                }
                return retain_publication_recovery(
                    instance_mods,
                    recovery,
                    InstallError::Transfer,
                );
            }
        }
    }
    staged.sort_by(|left, right| left.installed.project_id.cmp(&right.installed.project_id));
    Ok(staged)
}

fn retain_publication_recovery(
    instance_mods: &ManagedStorageDirectory,
    members: Vec<TransferRecoveryMember>,
    original: InstallError,
) -> Result<Vec<StagedManagedArtifact>, InstallError> {
    let effects = instance_mods.effect_owner().clone();
    let recovery = ManagedArtifactTransferRecovery {
        instance_mods: instance_mods.clone(),
        members,
    };
    match recovery.reconcile() {
        Ok(None) => Err(original),
        Ok(Some(recovery)) => Err(InstallError::Io(effects.retain_artifact_transfer(recovery))),
        Err((error, recovery)) => {
            let _pending = effects.retain_artifact_transfer(recovery);
            Err(InstallError::Io(error))
        }
    }
}

fn report_matches_artifact(report: &TransferReport, installed: &InstalledMod) -> bool {
    report.bytes() == installed.size
        && report
            .digests()
            .sha512()
            .is_some_and(|digest| hex::encode(digest) == installed.integrity.sha512)
}

fn invalid_transfer_plan() -> InstallError {
    InstallError::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        "managed artifact transfer does not match its sealed plan",
    ))
}

pub(super) fn installed_graph_from_plan(plan: &ManagedCompositionInstallPlan) -> Vec<InstalledMod> {
    plan.pins().iter().map(installed_from_pin).collect()
}

fn installed_from_pin(pin: &ManagedArtifactPin) -> InstalledMod {
    InstalledMod {
        project_id: pin.project_id().to_string(),
        version_id: pin.version_id().to_string(),
        filename: pin.filename().to_string(),
        role: pin.role(),
        size: pin.size(),
        ownership_class: OwnershipClass::CompositionManaged,
        source: ManagedArtifactSource {
            provider: ManagedArtifactProvider::Modrinth,
        },
        integrity: ManagedArtifactIntegrity {
            sha512: pin.sha512().to_string(),
        },
    }
}

pub(super) fn state_from_plan(
    plan: &ManagedCompositionInstallPlan,
    installed_mods: Vec<InstalledMod>,
) -> CompositionState {
    CompositionState {
        composition_id: plan.composition_id().to_string(),
        family: plan.family(),
        tier: plan.tier(),
        game_version: plan.game_version().to_string(),
        loader: plan.loader().to_string(),
        graph_sha512: plan.graph_digest().to_string(),
        dependency_edges: plan
            .edges()
            .iter()
            .map(|edge| ManagedDependencyStateEdge {
                parent_project_id: edge.parent_project_id().to_string(),
                child_project_id: edge.child_project_id().to_string(),
                child_version_id: edge.child_version_id().to_string(),
            })
            .collect(),
        installed_mods,
        installed_at: Utc::now().to_rfc3339(),
    }
}
