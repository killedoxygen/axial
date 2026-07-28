use crate::download::{
    AuthenticatedVersionBundleMemberSource, AuthenticatedVersionBundleSource,
    ManagedInstallActivationContractId, ManagedInstallPublicationCandidates,
};
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRelativePath, KnownGoodRoot,
    MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection,
    ManagedKnownGoodComponent,
};
use crate::loaders::LoaderError;
#[cfg(test)]
use crate::managed_fs::ManagedCreateOnlyWriteFault;
use crate::managed_fs::{
    ManagedCreateOnlyWriteFailure, ManagedDir, ManagedDirectoryIdentity, ManagedFileGuard,
    ManagedFileIdentity, ManagedGuardedFileMoveFailure,
};
use crate::managed_publication::{
    ManagedCanonicalState, ManagedPriorFingerprint as PriorFingerprint,
    ManagedPublicationDataError, ManagedRootPublicationLease, ManagedTargetPathError,
    authenticate_guarded_publication_file, bounded_marker_bytes, committed_terminal_shape_is_valid,
    exact_portable_names as exact_names, managed_directory_path_exists, open_managed_target_parent,
    read_bounded_marker, rollback_terminal_shape_is_reachable, run_publication_blocking,
    settled_terminal_shape_is_valid as managed_settled_terminal_shape_is_valid,
    valid_publication_nonce as valid_nonce, valid_publication_sha1 as valid_sha1,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::hash::Hasher;
#[cfg(any(test, feature = "test-support"))]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

const LANE_NAME: &str = "version-bundle";
const STAGING_NAME: &str = "staging";
const QUARANTINE_NAME: &str = "quarantine";
const INTENT_NAME: &str = "intent.json";
const OUTCOME_NAME: &str = "outcome.json";
const SETTLEMENT_NAME: &str = "settlement.json";
const MAX_VERSION_BUNDLE_ENTRIES: usize = 3;
const MAX_LANE_ENTRIES: usize = 5;
const MAX_MARKER_BYTES: usize = 16 << 10;
const MAX_RECOVERY_ATTEMPTS: usize = 8;
const INTENT_SCHEMA: &str = "axial.version_bundle_publication.intent.v3";
const OUTCOME_SCHEMA: &str = "axial.version_bundle_publication.outcome.v2";
const SETTLEMENT_SCHEMA: &str = "axial.version_bundle_publication.settlement.v4";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum PhysicalRoot {
    Versions,
    Assets,
}

impl PhysicalRoot {
    fn directory_name(self) -> &'static str {
        match self {
            Self::Versions => "versions",
            Self::Assets => "assets",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedArtifactKind {
    VersionMetadata,
    ClientJar,
    LogConfig,
}

impl PersistedArtifactKind {
    fn from_known_good(kind: KnownGoodArtifactKind) -> Option<Self> {
        match kind {
            KnownGoodArtifactKind::VersionMetadata => Some(Self::VersionMetadata),
            KnownGoodArtifactKind::ClientJar => Some(Self::ClientJar),
            KnownGoodArtifactKind::LogConfig => Some(Self::LogConfig),
            _ => None,
        }
    }

    fn known_good(self) -> KnownGoodArtifactKind {
        match self {
            Self::VersionMetadata => KnownGoodArtifactKind::VersionMetadata,
            Self::ClientJar => KnownGoodArtifactKind::ClientJar,
            Self::LogConfig => KnownGoodArtifactKind::LogConfig,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFingerprint {
    ordinal: usize,
    root: PhysicalRoot,
    path: KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
    digest: String,
    size: u64,
}

struct PlannedEntry {
    fingerprint: EntryFingerprint,
    source: AuthenticatedVersionBundleMemberSource,
    target: Option<CanonicalTarget>,
}

struct TransactionEntry {
    fingerprint: EntryFingerprint,
    stage_name: String,
    quarantine_name: String,
    stage_guard: Option<ManagedFileGuard>,
    canonical_guard: Option<ManagedFileGuard>,
    target: Option<CanonicalTarget>,
    state: EntryState,
}

struct CanonicalTarget {
    parent: ManagedDir,
    name: String,
    previous: Option<ManagedFileGuard>,
    prior_fingerprint: PriorFingerprint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryState {
    Prepared,
    AlreadyExact,
    Quarantined,
    PublishedNew,
    PublishedReplacement,
    RolledBack,
    RollbackUncertain,
}

struct TransactionContext {
    lease: ManagedRootPublicationLease,
    root_identity: ManagedDirectoryIdentity,
    lane: ManagedDir,
    staging: ManagedDir,
    quarantine: ManagedDir,
    intent: PersistedIntent,
    intent_guard: ManagedFileGuard,
    outcome_guard: Option<ManagedFileGuard>,
    entries: Vec<TransactionEntry>,
    #[cfg(any(test, feature = "test-support"))]
    test_hook: Option<PublicationTestHook>,
}

struct TransactionHandles {
    lease: ManagedRootPublicationLease,
    lane: ManagedDir,
    staging: ManagedDir,
    quarantine: ManagedDir,
    intent: PersistedIntent,
    intent_guard: ManagedFileGuard,
}

#[cfg(any(test, feature = "test-support"))]
enum PublicationTestHook {
    FailAfter {
        promotions: usize,
    },
    #[cfg(test)]
    PauseAfter {
        promotions: usize,
        reached: Option<tokio::sync::oneshot::Sender<()>>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    },
    #[cfg(test)]
    CrashAfterPromotion {
        kind: KnownGoodArtifactKind,
    },
    #[cfg(test)]
    ReportFirstMoveUnsettled,
    #[cfg(test)]
    IntentWriteFault(ManagedCreateOnlyWriteFault),
    #[cfg(test)]
    FailAfterIntent,
    #[cfg(test)]
    FailSettlementOnce,
    #[cfg(test)]
    FailSettlementPermanently,
    #[cfg(test)]
    SettlementWriteFault(ManagedCreateOnlyWriteFault),
    #[cfg(test)]
    FailAfterSettlementMarkerOnce,
    #[cfg(test)]
    FailAfterCommittedOutcomeOnce,
}

#[cfg(any(test, feature = "test-support"))]
static TEST_HOOKS: OnceLock<Mutex<HashMap<String, PublicationTestHook>>> = OnceLock::new();

pub(crate) struct VersionBundleTransactionCommitReceipt {
    context: Arc<TransactionContext>,
}

pub(crate) struct VersionBundleTransactionFailureReceipt {
    context: Arc<TransactionContext>,
    expectation: SettlementExpectation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettlementExpectation {
    Proven(PersistedTerminalOutcome),
    PendingFailure {
        effect: VersionBundleTransactionEffect,
    },
}

pub(crate) enum VersionBundleTransactionSettledOutcome {
    Committed(ManagedRootPublicationLease),
    RolledBack {
        lease: ManagedRootPublicationLease,
        effect: VersionBundleTransactionEffect,
    },
}

pub(crate) struct DurableVersionBundleEvidence {
    settlement: PersistedSettlement,
    settlement_identity: ManagedFileIdentity,
    root_binding: String,
    fingerprint: String,
}

pub(crate) enum DurableVersionBundleOutcome {
    NoEffect(ManagedRootPublicationLease),
    Committed {
        lease: ManagedRootPublicationLease,
        evidence: DurableVersionBundleEvidence,
    },
    RolledBack {
        lease: ManagedRootPublicationLease,
        evidence: DurableVersionBundleEvidence,
        effect: VersionBundleTransactionEffect,
    },
    Indeterminate(ManagedRootPublicationLease),
}

pub(crate) enum DurableVersionBundleAcknowledgementOutcome {
    Acknowledged(ManagedRootPublicationLease),
    Indeterminate {
        lease: ManagedRootPublicationLease,
        evidence: DurableVersionBundleEvidence,
    },
}

enum DurableVersionBundleClassification {
    NoEffect,
    Committed(DurableVersionBundleEvidence),
    RolledBack {
        evidence: DurableVersionBundleEvidence,
        effect: VersionBundleTransactionEffect,
    },
}

impl DurableVersionBundleEvidence {
    pub(crate) fn version_id(&self) -> &str {
        &self.settlement.intent.version_id
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn root_binding(&self) -> &str {
        &self.root_binding
    }

    pub(crate) fn transaction_nonce(&self) -> &str {
        &self.settlement.intent.transaction_nonce
    }

    pub(crate) fn settlement_generation(&self) -> &str {
        &self.settlement.generation_nonce
    }

    pub(crate) fn committed_activation_contract_id(
        &self,
    ) -> Option<&ManagedInstallActivationContractId> {
        validate_settlement(&self.settlement).ok()?;
        if self.settlement.outcome.outcome != PersistedTerminalOutcome::Committed {
            return None;
        }
        Some(&self.settlement.intent.activation_contract_id)
    }
}

impl std::fmt::Debug for VersionBundleTransactionSettledOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Committed(_) => "VersionBundleTransactionSettledOutcome::Committed(..)",
            Self::RolledBack { .. } => "VersionBundleTransactionSettledOutcome::RolledBack(..)",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VersionBundleTransactionEffect {
    Promotion,
    Postcheck,
    Rollback,
}

impl std::fmt::Debug for VersionBundleTransactionCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VersionBundleTransactionCommitReceipt")
            .field("entry_count", &self.context.entries.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for VersionBundleTransactionFailureReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VersionBundleTransactionFailureReceipt")
            .field("entry_count", &self.context.entries.len())
            .field("expectation", &self.expectation)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum VersionBundleTransactionError {
    #[error("version bundle source does not match the admitted projection")]
    ProjectionMismatch,
    #[error("version bundle projection contains a portable path alias")]
    PortablePathAlias,
    #[error("version bundle publication lane belongs to another exact projection")]
    LaneOccupied,
    #[error("version bundle publication has an unacknowledged durable settlement")]
    UnacknowledgedSettlement,
    #[error("version bundle publication recovery is ambiguous")]
    RecoveryAmbiguous,
    #[error("version bundle publication preparation failed")]
    Preparation,
    #[error("version bundle publication task stopped unexpectedly")]
    TaskStopped,
    #[error("version bundle publication remains indeterminate")]
    Indeterminate(VersionBundleTransactionRecovery),
    #[error("version bundle publication recovery could not prove an effect")]
    RecoveryUnsettled,
    #[error("version bundle publication effects failed")]
    Effect(Box<VersionBundleTransactionFailureReceipt>),
}

#[must_use = "dropping recovery releases the exclusive publication authority"]
pub(crate) struct VersionBundleTransactionRecovery {
    state: VersionBundleTransactionRecoveryState,
}

enum VersionBundleTransactionRecoveryState {
    Preparation(VersionBundleTransactionPreparationRecovery),
    Settlement(VersionBundleTransactionSettlementRetry),
}

struct VersionBundleTransactionPreparationRecovery {
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    version_id: String,
    activation_contract_id: ManagedInstallActivationContractId,
    fingerprints: Vec<EntryFingerprint>,
}

impl std::fmt::Debug for VersionBundleTransactionRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            VersionBundleTransactionRecoveryState::Preparation(_) => "preparation",
            VersionBundleTransactionRecoveryState::Settlement(_) => "settlement",
        };
        formatter
            .debug_struct("VersionBundleTransactionRecovery")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

impl VersionBundleTransactionRecovery {
    pub(crate) async fn retry(
        self,
    ) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionError> {
        match self.state {
            VersionBundleTransactionRecoveryState::Preparation(recovery) => {
                let publication = continue_version_bundle_publication(
                    recovery.lease,
                    recovery.source,
                    recovery.version_id,
                    recovery.activation_contract_id,
                    recovery.fingerprints,
                    #[cfg(any(test, feature = "test-support"))]
                    None,
                )
                .await;
                settle_version_bundle_publication(publication).await
            }
            VersionBundleTransactionRecoveryState::Settlement(retry) => {
                settle_version_bundle_progress(VersionBundleSettlementProgress::Retry(retry)).await
            }
        }
    }
}

pub(crate) struct VersionBundleTransactionSettlementRetry {
    context: Arc<TransactionContext>,
    expectation: SettlementExpectation,
}

impl std::fmt::Debug for VersionBundleTransactionSettlementRetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VersionBundleTransactionSettlementRetry")
            .field("expectation", &self.expectation)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for VersionBundleTransactionSettlementRetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("version bundle receipt settlement remains retryable")
    }
}

impl std::error::Error for VersionBundleTransactionSettlementRetry {}

impl VersionBundleTransactionSettlementRetry {
    pub(crate) async fn retry(
        self,
    ) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionSettlementRetry>
    {
        let context = Arc::try_unwrap(self.context).map_err(|context| Self {
            context,
            expectation: self.expectation,
        })?;
        settle_owned_context(context, self.expectation).await
    }
}

impl From<ManagedPublicationDataError> for VersionBundleTransactionError {
    fn from(_: ManagedPublicationDataError) -> Self {
        Self::RecoveryAmbiguous
    }
}

impl VersionBundleTransactionCommitReceipt {
    pub(crate) async fn settle(
        self,
    ) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionSettlementRetry>
    {
        let context = Arc::try_unwrap(self.context).map_err(|context| {
            VersionBundleTransactionSettlementRetry {
                context,
                expectation: SettlementExpectation::Proven(PersistedTerminalOutcome::Committed),
            }
        })?;
        settle_owned_context(
            context,
            SettlementExpectation::Proven(PersistedTerminalOutcome::Committed),
        )
        .await
    }
}

impl VersionBundleTransactionFailureReceipt {
    pub(crate) async fn settle(
        self,
    ) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionSettlementRetry>
    {
        let expectation = self.expectation;
        let context = Arc::try_unwrap(self.context).map_err(|context| {
            VersionBundleTransactionSettlementRetry {
                context,
                expectation,
            }
        })?;
        settle_owned_context(context, expectation).await
    }
}

enum PreparationOutcome {
    Ready(Box<TransactionContext>),
    Committed(VersionBundleTransactionCommitReceipt),
    RolledBack(VersionBundleTransactionFailureReceipt),
}

pub(crate) async fn publish_version_bundle(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    activation_contract_id: ManagedInstallActivationContractId,
    projection: ManagedComponentProjection<'_>,
) -> Result<VersionBundleTransactionCommitReceipt, VersionBundleTransactionError> {
    if !source.matches_projection(&projection) {
        return Err(VersionBundleTransactionError::ProjectionMismatch);
    }
    let version_id = source.version_id().to_string();
    #[cfg(any(test, feature = "test-support"))]
    let test_hook = TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&version_id);
    let fingerprints = own_fingerprints(&projection)?;
    validate_portable_aliases(&fingerprints)?;
    validate_bundle_topology(&version_id, &fingerprints)?;
    continue_version_bundle_publication(
        lease,
        source,
        version_id,
        activation_contract_id,
        fingerprints,
        #[cfg(any(test, feature = "test-support"))]
        test_hook,
    )
    .await
}

async fn continue_version_bundle_publication(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    version_id: String,
    activation_contract_id: ManagedInstallActivationContractId,
    fingerprints: Vec<EntryFingerprint>,
    #[cfg(any(test, feature = "test-support"))] test_hook: Option<PublicationTestHook>,
) -> Result<VersionBundleTransactionCommitReceipt, VersionBundleTransactionError> {
    let mut lease = lease;
    #[cfg(any(test, feature = "test-support"))]
    let mut test_hook = test_hook;
    let mut retry_delay = std::time::Duration::from_millis(25);
    let maximum_retry_delay = std::time::Duration::from_secs(1);
    let mut recovery_attempts = 0usize;
    let preparation = loop {
        let recovery = lease.retain_recovery();
        let attempt_source = source.clone();
        let attempt_version_id = version_id.clone();
        let attempt_activation_contract_id = activation_contract_id.clone();
        let attempt_fingerprints = fingerprints.clone();
        #[cfg(any(test, feature = "test-support"))]
        let attempt_test_hook = test_hook.take();
        #[cfg(any(test, feature = "test-support"))]
        let attempt = run_publication_blocking(move || {
            prepare_transaction(
                lease,
                attempt_source,
                attempt_version_id,
                attempt_activation_contract_id,
                attempt_fingerprints,
                attempt_test_hook,
            )
        })
        .await;
        #[cfg(not(any(test, feature = "test-support")))]
        let attempt = run_publication_blocking(move || {
            prepare_transaction(
                lease,
                attempt_source,
                attempt_version_id,
                attempt_activation_contract_id,
                attempt_fingerprints,
            )
        })
        .await;
        match attempt {
            Ok(Ok(preparation)) => break preparation,
            Ok(Err(
                error @ (VersionBundleTransactionError::ProjectionMismatch
                | VersionBundleTransactionError::PortablePathAlias
                | VersionBundleTransactionError::LaneOccupied
                | VersionBundleTransactionError::UnacknowledgedSettlement),
            )) => {
                let _ = recovery.restore();
                return Err(error);
            }
            Ok(Err(VersionBundleTransactionError::RecoveryUnsettled)) => {
                lease = recovery.restore();
                return Err(VersionBundleTransactionError::Indeterminate(
                    VersionBundleTransactionRecovery {
                        state: VersionBundleTransactionRecoveryState::Preparation(
                            VersionBundleTransactionPreparationRecovery {
                                lease,
                                source,
                                version_id,
                                activation_contract_id,
                                fingerprints,
                            },
                        ),
                    },
                ));
            }
            Ok(Err(error)) => {
                lease = recovery.restore();
                if version_bundle_effect_boundary(&lease) == Some(false) {
                    return Err(error);
                }
            }
            Err(_) => {
                lease = recovery.restore();
                if version_bundle_effect_boundary(&lease) == Some(false) {
                    return Err(VersionBundleTransactionError::TaskStopped);
                }
            }
        }
        recovery_attempts = recovery_attempts.saturating_add(1);
        if recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
            return Err(VersionBundleTransactionError::Indeterminate(
                VersionBundleTransactionRecovery {
                    state: VersionBundleTransactionRecoveryState::Preparation(
                        VersionBundleTransactionPreparationRecovery {
                            lease,
                            source,
                            version_id,
                            activation_contract_id,
                            fingerprints,
                        },
                    ),
                },
            ));
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
    };
    let context = match preparation {
        PreparationOutcome::Ready(context) => *context,
        PreparationOutcome::Committed(receipt) => return Ok(receipt),
        PreparationOutcome::RolledBack(receipt) => {
            return Err(VersionBundleTransactionError::Effect(Box::new(receipt)));
        }
    };

    mutate_owned_context(context)
        .await
        .map_err(|receipt| VersionBundleTransactionError::Effect(Box::new(receipt)))
}

enum VersionBundleSettlementProgress {
    Commit(VersionBundleTransactionCommitReceipt),
    Failure(VersionBundleTransactionFailureReceipt),
    Retry(VersionBundleTransactionSettlementRetry),
}

pub(crate) async fn settle_version_bundle_publication(
    publication: Result<VersionBundleTransactionCommitReceipt, VersionBundleTransactionError>,
) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionError> {
    let progress = match publication {
        Ok(receipt) => VersionBundleSettlementProgress::Commit(receipt),
        Err(VersionBundleTransactionError::Effect(receipt)) => {
            VersionBundleSettlementProgress::Failure(*receipt)
        }
        Err(error) => return Err(error),
    };
    settle_version_bundle_progress(progress).await
}

async fn settle_version_bundle_progress(
    mut progress: VersionBundleSettlementProgress,
) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionError> {
    let mut retry_delay = std::time::Duration::from_millis(25);
    let maximum_retry_delay = std::time::Duration::from_secs(1);
    let mut recovery_attempts = 0usize;
    loop {
        let attempted = match progress {
            VersionBundleSettlementProgress::Commit(receipt) => receipt.settle().await,
            VersionBundleSettlementProgress::Failure(receipt) => receipt.settle().await,
            VersionBundleSettlementProgress::Retry(retry) => retry.retry().await,
        };
        match attempted {
            Ok(outcome) => return Ok(outcome),
            Err(retry) => {
                recovery_attempts = recovery_attempts.saturating_add(1);
                if recovery_attempts >= MAX_RECOVERY_ATTEMPTS {
                    return Err(VersionBundleTransactionError::Indeterminate(
                        VersionBundleTransactionRecovery {
                            state: VersionBundleTransactionRecoveryState::Settlement(retry),
                        },
                    ));
                }
                progress = VersionBundleSettlementProgress::Retry(retry);
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
            }
        }
    }
}

pub(crate) async fn classify_durable_version_bundle_candidates(
    lease: ManagedRootPublicationLease,
    candidates: ManagedInstallPublicationCandidates,
) -> DurableVersionBundleOutcome {
    let holder = Arc::new(Mutex::new(Some(lease)));
    let worker_holder = Arc::clone(&holder);
    let attempted = run_publication_blocking(move || {
        let lease = worker_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("durable classifier lease is present");
        let outcome = classify_durable_version_bundle_owned(lease, &candidates);
        if let Err(lease) = outcome {
            *worker_holder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lease);
            None
        } else {
            outcome.ok()
        }
    })
    .await
    .ok()
    .flatten();
    if let Some(outcome) = attempted {
        return outcome;
    }
    DurableVersionBundleOutcome::Indeterminate(
        holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("durable classifier restored its lease"),
    )
}

fn classify_durable_version_bundle_owned(
    lease: ManagedRootPublicationLease,
    candidates: &ManagedInstallPublicationCandidates,
) -> Result<DurableVersionBundleOutcome, ManagedRootPublicationLease> {
    let classified = (|| {
        lease
            .root()
            .settle()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        lease
            .revalidate()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        let publication = lease.publication_directory();
        if !publication
            .has_portably_exact_child_name(LANE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
        {
            lease
                .revalidate()
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
            return Ok(DurableVersionBundleClassification::NoEffect);
        }
        let lane = publication
            .open_child(LANE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        lane.sweep_orphan_temps()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        if let Some((settlement, settlement_guard)) = read_settlement(&lane)? {
            return durable_classification_from_settlement(
                &lease,
                &lane,
                candidates,
                settlement,
                settlement_guard,
            );
        }
        let Some((intent, intent_guard)) = read_intent(&lane)? else {
            require_empty_lane(&lane)?;
            lease
                .revalidate()
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
            return Ok(DurableVersionBundleClassification::NoEffect);
        };
        validate_persisted_intent(&intent)?;
        if !candidates.contains(&intent.version_id)
            || !lane
                .file_guard_matches(INTENT_NAME, &intent_guard)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
        {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
        let (staging, quarantine) = open_or_create_slots_after_intent(&lease, &lane)?;
        let outcome = if let Some((outcome, outcome_guard)) = read_outcome(&lane)? {
            validate_outcome(&outcome, &intent)?;
            if !lane
                .file_guard_matches(OUTCOME_NAME, &outcome_guard)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            {
                return Err(VersionBundleTransactionError::RecoveryAmbiguous);
            }
            validate_durable_terminal_shape(
                &lease,
                &staging,
                &quarantine,
                &intent,
                outcome.outcome,
            )?;
            outcome
        } else {
            let outcome = match reconcile_unfinished_moves(&lease, &staging, &quarantine, &intent)?
            {
                UnfinishedMoveOutcome::Committed => PersistedTerminalOutcome::Committed,
                UnfinishedMoveOutcome::RolledBack => PersistedTerminalOutcome::RolledBack {
                    effect: VersionBundleTransactionEffect::Rollback,
                },
            };
            let outcome_guard = write_outcome(&lane, &intent, outcome)
                .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?;
            if !lane
                .file_guard_matches(OUTCOME_NAME, &outcome_guard)
                .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?
            {
                return Err(VersionBundleTransactionError::RecoveryUnsettled);
            }
            PersistedOutcome {
                schema: OUTCOME_SCHEMA.to_string(),
                transaction_nonce: intent.transaction_nonce.clone(),
                outcome,
            }
        };
        let settlement = PersistedSettlement {
            schema: SETTLEMENT_SCHEMA.to_string(),
            phase: PersistedSettlementPhase::CallerSettled,
            generation_nonce: uuid::Uuid::new_v4().simple().to_string(),
            intent,
            outcome,
        };
        let settlement_guard = write_settlement(&lane, &settlement)
            .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?;
        durable_classification_from_settlement(
            &lease,
            &lane,
            candidates,
            settlement,
            settlement_guard,
        )
    })();
    match classified {
        Ok(DurableVersionBundleClassification::NoEffect) => {
            Ok(DurableVersionBundleOutcome::NoEffect(lease))
        }
        Ok(DurableVersionBundleClassification::Committed(evidence)) => {
            Ok(DurableVersionBundleOutcome::Committed { lease, evidence })
        }
        Ok(DurableVersionBundleClassification::RolledBack { evidence, effect }) => {
            Ok(DurableVersionBundleOutcome::RolledBack {
                lease,
                evidence,
                effect,
            })
        }
        Err(_) => Err(lease),
    }
}

pub(crate) async fn acknowledge_durable_version_bundle(
    lease: ManagedRootPublicationLease,
    evidence: DurableVersionBundleEvidence,
) -> DurableVersionBundleAcknowledgementOutcome {
    let holder = Arc::new(Mutex::new(Some((lease, evidence))));
    let worker_holder = Arc::clone(&holder);
    let attempted = run_publication_blocking(move || {
        let (lease, evidence) = worker_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("durable acknowledgement authority is present");
        match acknowledge_durable_version_bundle_owned(lease, &evidence) {
            Ok(lease) => Some(DurableVersionBundleAcknowledgementOutcome::Acknowledged(
                lease,
            )),
            Err(lease) => {
                *worker_holder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((lease, evidence));
                None
            }
        }
    })
    .await
    .ok()
    .flatten();
    if let Some(outcome) = attempted {
        return outcome;
    }
    let (lease, evidence) = holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("durable acknowledgement restored its authority");
    DurableVersionBundleAcknowledgementOutcome::Indeterminate { lease, evidence }
}

fn acknowledge_durable_version_bundle_owned(
    lease: ManagedRootPublicationLease,
    evidence: &DurableVersionBundleEvidence,
) -> Result<ManagedRootPublicationLease, ManagedRootPublicationLease> {
    let acknowledged = (|| {
        lease
            .root()
            .settle()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        lease
            .revalidate()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        let publication = lease.publication_directory();
        let lane = publication
            .open_child(LANE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        match read_settlement(&lane)? {
            Some((settlement, settlement_guard)) => {
                let root_binding = durable_version_bundle_root_binding(
                    lease.root(),
                    &settlement.intent.transaction_nonce,
                    &settlement.generation_nonce,
                )
                .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                if settlement != evidence.settlement
                    || settlement_guard.identity() != evidence.settlement_identity
                    || root_binding != evidence.root_binding
                    || settlement_fingerprint(&root_binding, &settlement)? != evidence.fingerprint
                    || !lane
                        .file_guard_matches(SETTLEMENT_NAME, &settlement_guard)
                        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                cleanup_settled_lane_contents(&lease, &lane, &settlement)?;
                if !lane
                    .file_guard_matches(SETTLEMENT_NAME, &settlement_guard)
                    .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                lane.remove_guarded_file(SETTLEMENT_NAME, &settlement_guard)
                    .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
            }
            None => {
                require_empty_lane(&lane)?;
                validate_clean_settled_terminal_shape(&lease, &lane, &evidence.settlement)?;
            }
        }
        lease
            .revalidate()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        Ok(())
    })();
    match acknowledged {
        Ok(()) => Ok(lease),
        Err(_) => Err(lease),
    }
}

fn prepare_transaction(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    version_id: String,
    activation_contract_id: ManagedInstallActivationContractId,
    fingerprints: Vec<EntryFingerprint>,
    #[cfg(any(test, feature = "test-support"))] test_hook: Option<PublicationTestHook>,
) -> Result<PreparationOutcome, VersionBundleTransactionError> {
    #[cfg(test)]
    let mut test_hook = test_hook;
    let mut planned = bind_sources(source, fingerprints)?;
    let lane = open_lane(&lease)?;
    refuse_unacknowledged_settlement(&lane)?;

    if let Some((intent, intent_guard)) = read_intent(&lane)? {
        if !intent_matches_projection(&intent, &version_id, &activation_contract_id, &planned)? {
            return Err(VersionBundleTransactionError::LaneOccupied);
        }
        let (staging, quarantine) = open_or_create_slots_after_intent(&lease, &lane)?;
        if let Some((outcome, outcome_guard)) = read_outcome(&lane)? {
            let terminal = outcome.outcome;
            let context = reconstruct_terminal_context(
                TransactionHandles {
                    lease,
                    lane,
                    staging,
                    quarantine,
                    intent,
                    intent_guard,
                },
                outcome,
                outcome_guard,
                #[cfg(any(test, feature = "test-support"))]
                test_hook,
            )?;
            return match terminal {
                PersistedTerminalOutcome::Committed => {
                    Ok(PreparationOutcome::Committed(committed_receipt(context)))
                }
                PersistedTerminalOutcome::RolledBack { effect } => Ok(
                    PreparationOutcome::RolledBack(VersionBundleTransactionFailureReceipt {
                        context: Arc::new(context),
                        expectation: SettlementExpectation::Proven(
                            PersistedTerminalOutcome::RolledBack { effect },
                        ),
                    }),
                ),
            };
        }
        if let Some(outcome_guard) =
            recover_unfinished_commit(&lease, &lane, &staging, &quarantine, &intent, &planned)?
        {
            let (outcome, observed_outcome_guard) = read_outcome(&lane)
                .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?
                .ok_or(VersionBundleTransactionError::RecoveryUnsettled)?;
            if observed_outcome_guard.identity() != outcome_guard.identity()
                || !lane
                    .file_guard_matches(OUTCOME_NAME, &outcome_guard)
                    .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?
            {
                return Err(VersionBundleTransactionError::RecoveryUnsettled);
            }
            let context = reconstruct_terminal_context(
                TransactionHandles {
                    lease,
                    lane,
                    staging,
                    quarantine,
                    intent,
                    intent_guard,
                },
                outcome,
                outcome_guard,
                #[cfg(any(test, feature = "test-support"))]
                test_hook,
            )?;
            return Ok(PreparationOutcome::Committed(committed_receipt(context)));
        }
        let root_identity = lease
            .root()
            .identity()
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        let context = context_from_prepared(
            TransactionHandles {
                lease,
                lane,
                staging,
                quarantine,
                intent,
                intent_guard,
            },
            root_identity,
            &mut planned,
            #[cfg(any(test, feature = "test-support"))]
            test_hook,
        )?;
        return Ok(PreparationOutcome::Ready(Box::new(context)));
    }
    if read_outcome(&lane)?.is_some() {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    require_empty_lane(&lane)?;
    let expected = planned
        .iter()
        .map(|entry| entry.fingerprint.clone())
        .collect::<Vec<_>>();
    validate_existing_portable_paths(lease.root(), &expected)?;
    let (targets, created_ancestors) = preflight_canonical_targets(lease.root(), &expected)?;
    for (planned, target) in planned.iter_mut().zip(targets) {
        planned.target = target;
    }
    let intent = persisted_intent(
        &version_id,
        activation_contract_id,
        &planned,
        created_ancestors,
    )?;
    let intent_bytes = bounded_marker_bytes(&intent, MAX_MARKER_BYTES)
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    #[cfg(test)]
    let intent_write_fault = match test_hook.as_ref() {
        Some(PublicationTestHook::IntentWriteFault(fault)) => Some(*fault),
        _ => None,
    };
    #[cfg(test)]
    if intent_write_fault.is_some() {
        test_hook = None;
    }
    #[cfg(test)]
    let intent_write = match intent_write_fault {
        Some(fault) => lane.write_new_exact_retained_with_fault(INTENT_NAME, &intent_bytes, fault),
        None => lane.write_new_exact_retained(INTENT_NAME, &intent_bytes),
    };
    #[cfg(not(test))]
    let intent_write = lane.write_new_exact_retained(INTENT_NAME, &intent_bytes);
    let intent_guard = match intent_write {
        Ok(guard) => guard,
        Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_)) => {
            return Err(VersionBundleTransactionError::Preparation);
        }
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { .. }) => {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    };
    #[cfg(test)]
    if matches!(test_hook, Some(PublicationTestHook::FailAfterIntent)) {
        return Err(VersionBundleTransactionError::Preparation);
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    let (staging, quarantine) = open_or_create_slots_after_intent(&lease, &lane)?;
    let root_identity = lease
        .root()
        .identity()
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    context_from_prepared(
        TransactionHandles {
            lease,
            lane,
            staging,
            quarantine,
            intent,
            intent_guard,
        },
        root_identity,
        &mut planned,
        #[cfg(any(test, feature = "test-support"))]
        test_hook,
    )
    .map(Box::new)
    .map(PreparationOutcome::Ready)
}

fn bind_sources(
    source: AuthenticatedVersionBundleSource,
    fingerprints: Vec<EntryFingerprint>,
) -> Result<Vec<PlannedEntry>, VersionBundleTransactionError> {
    let mut sources = source.into_sources();
    let mut planned = Vec::with_capacity(fingerprints.len());
    for fingerprint in fingerprints {
        let source_index = sources
            .iter()
            .position(|source| source.kind() == fingerprint.kind)
            .ok_or(VersionBundleTransactionError::ProjectionMismatch)?;
        planned.push(PlannedEntry {
            fingerprint,
            source: sources.remove(source_index),
            target: None,
        });
    }
    if !sources.is_empty() || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&planned.len()) {
        return Err(VersionBundleTransactionError::ProjectionMismatch);
    }
    Ok(planned)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedIntent {
    schema: String,
    phase: PersistedIntentPhase,
    version_id: String,
    activation_contract_id: ManagedInstallActivationContractId,
    transaction_nonce: String,
    created_ancestors: Vec<String>,
    entries: Vec<PersistedEntry>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedIntentPhase {
    Prepared,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedEntry {
    ordinal: usize,
    root: PhysicalRoot,
    relative_path: String,
    kind: PersistedArtifactKind,
    source_sha1: String,
    source_size: u64,
    staging_slot: String,
    quarantine_slot: String,
    prior: PriorFingerprint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedOutcome {
    schema: String,
    transaction_nonce: String,
    outcome: PersistedTerminalOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedTerminalOutcome {
    Committed,
    RolledBack {
        effect: VersionBundleTransactionEffect,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedSettlement {
    schema: String,
    phase: PersistedSettlementPhase,
    generation_nonce: String,
    intent: PersistedIntent,
    outcome: PersistedOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedSettlementPhase {
    CallerSettled,
}

fn own_fingerprints(
    projection: &ManagedComponentProjection<'_>,
) -> Result<Vec<EntryFingerprint>, VersionBundleTransactionError> {
    if projection.component() != ManagedKnownGoodComponent::VersionBundle
        || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&projection.entry_count())
        || projection.expected_content_byte_count() > MAX_TIER2_AGGREGATE_BYTES
    {
        return Err(VersionBundleTransactionError::ProjectionMismatch);
    }
    projection
        .entries()
        .iter()
        .map(|projected| {
            let entry = projected.entry();
            let root = match entry.root() {
                KnownGoodRoot::Versions => PhysicalRoot::Versions,
                KnownGoodRoot::Assets => PhysicalRoot::Assets,
                KnownGoodRoot::Libraries | KnownGoodRoot::ManagedRuntime { .. } => {
                    return Err(VersionBundleTransactionError::ProjectionMismatch);
                }
            };
            let (digest, size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => {
                    (digest.as_str().to_string(), *size)
                }
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(VersionBundleTransactionError::ProjectionMismatch);
                }
            };
            if size == 0 || size > MAX_TIER2_ARTIFACT_BYTES || !valid_sha1(&digest) {
                return Err(VersionBundleTransactionError::ProjectionMismatch);
            }
            Ok(EntryFingerprint {
                ordinal: projected.inventory_ordinal(),
                root,
                path: entry.path().clone(),
                kind: entry.kind(),
                digest,
                size,
            })
        })
        .collect()
}

fn validate_portable_aliases(
    fingerprints: &[EntryFingerprint],
) -> Result<(), VersionBundleTransactionError> {
    let mut paths = BTreeSet::new();
    for fingerprint in fingerprints {
        let portable_path =
            crate::portable_path::PortableRelativePath::new(fingerprint.path.as_str())
                .map_err(|_| VersionBundleTransactionError::ProjectionMismatch)?;
        let portable = (fingerprint.root, portable_path.key());
        if !paths.insert(portable) {
            return Err(VersionBundleTransactionError::PortablePathAlias);
        }
    }
    Ok(())
}

fn validate_bundle_topology(
    version_id: &str,
    fingerprints: &[EntryFingerprint],
) -> Result<(), VersionBundleTransactionError> {
    let safe_version = KnownGoodRelativePath::new(version_id)
        .map_err(|_| VersionBundleTransactionError::ProjectionMismatch)?;
    if safe_version.as_str().contains('/') || fingerprints.len() < 2 || fingerprints.len() > 3 {
        return Err(VersionBundleTransactionError::ProjectionMismatch);
    }
    let mut ordinals = BTreeSet::new();
    let mut metadata = 0;
    let mut client = 0;
    let mut log = 0;
    for entry in fingerprints {
        if !ordinals.insert(entry.ordinal) {
            return Err(VersionBundleTransactionError::ProjectionMismatch);
        }
        match entry.kind {
            KnownGoodArtifactKind::VersionMetadata
                if entry.root == PhysicalRoot::Versions
                    && entry.path.as_str() == format!("{version_id}/{version_id}.json") =>
            {
                metadata += 1;
            }
            KnownGoodArtifactKind::ClientJar
                if entry.root == PhysicalRoot::Versions
                    && entry.path.as_str() == format!("{version_id}/{version_id}.jar") =>
            {
                client += 1;
            }
            KnownGoodArtifactKind::LogConfig if entry.root == PhysicalRoot::Assets => {
                let mut segments = entry.path.as_str().split('/');
                if segments.next() != Some("log_configs")
                    || segments.next().is_none()
                    || segments.next().is_some()
                {
                    return Err(VersionBundleTransactionError::ProjectionMismatch);
                }
                log += 1;
            }
            _ => return Err(VersionBundleTransactionError::ProjectionMismatch),
        }
    }
    if metadata != 1 || client != 1 || log > 1 {
        return Err(VersionBundleTransactionError::ProjectionMismatch);
    }
    Ok(())
}

fn persisted_intent(
    version_id: &str,
    activation_contract_id: ManagedInstallActivationContractId,
    planned: &[PlannedEntry],
    created_ancestors: Vec<String>,
) -> Result<PersistedIntent, VersionBundleTransactionError> {
    let intent = PersistedIntent {
        schema: INTENT_SCHEMA.to_string(),
        phase: PersistedIntentPhase::Prepared,
        version_id: version_id.to_string(),
        activation_contract_id,
        transaction_nonce: uuid::Uuid::new_v4().simple().to_string(),
        created_ancestors,
        entries: planned
            .iter()
            .enumerate()
            .map(|(index, entry)| PersistedEntry {
                ordinal: entry.fingerprint.ordinal,
                root: entry.fingerprint.root,
                relative_path: entry.fingerprint.path.as_str().to_string(),
                kind: PersistedArtifactKind::from_known_good(entry.fingerprint.kind)
                    .expect("validated version bundle kind"),
                source_sha1: entry.fingerprint.digest.clone(),
                source_size: entry.fingerprint.size,
                staging_slot: format!("entry-{index}"),
                quarantine_slot: format!("entry-{index}"),
                prior: entry
                    .target
                    .as_ref()
                    .map(|target| target.prior_fingerprint.clone())
                    .unwrap_or(PriorFingerprint::Absent),
            })
            .collect(),
    };
    validate_persisted_intent(&intent)?;
    Ok(intent)
}

fn validate_persisted_intent(
    intent: &PersistedIntent,
) -> Result<Vec<EntryFingerprint>, VersionBundleTransactionError> {
    if intent.schema != INTENT_SCHEMA
        || intent.phase != PersistedIntentPhase::Prepared
        || !valid_nonce(&intent.transaction_nonce)
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let safe_version = KnownGoodRelativePath::new(&intent.version_id)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    if safe_version.as_str().contains('/')
        || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&intent.entries.len())
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let mut total_source = 0_u64;
    let mut total_prior = 0_u64;
    let mut fingerprints = Vec::with_capacity(intent.entries.len());
    for (index, entry) in intent.entries.iter().enumerate() {
        if entry.staging_slot != format!("entry-{index}")
            || entry.quarantine_slot != format!("entry-{index}")
            || entry.source_size == 0
            || entry.source_size > MAX_TIER2_ARTIFACT_BYTES
            || !valid_sha1(&entry.source_sha1)
        {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
        let path = KnownGoodRelativePath::new(&entry.relative_path)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        match &entry.prior {
            PriorFingerprint::Absent => {}
            PriorFingerprint::ExistingFile { sha1, size }
                if *size <= MAX_TIER2_ARTIFACT_BYTES && valid_sha1(sha1) =>
            {
                total_prior = total_prior
                    .checked_add(*size)
                    .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
            }
            PriorFingerprint::ExistingFile { .. } => {
                return Err(VersionBundleTransactionError::RecoveryAmbiguous);
            }
        }
        total_source = total_source
            .checked_add(entry.source_size)
            .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
        fingerprints.push(EntryFingerprint {
            ordinal: entry.ordinal,
            root: entry.root,
            path,
            kind: entry.kind.known_good(),
            digest: entry.source_sha1.clone(),
            size: entry.source_size,
        });
    }
    if total_source > MAX_TIER2_AGGREGATE_BYTES || total_prior > MAX_TIER2_AGGREGATE_BYTES {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    validate_portable_aliases(&fingerprints)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    validate_bundle_topology(&intent.version_id, &fingerprints)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    validate_created_ancestors(intent, &fingerprints)?;
    Ok(fingerprints)
}

fn validate_created_ancestors(
    intent: &PersistedIntent,
    fingerprints: &[EntryFingerprint],
) -> Result<(), VersionBundleTransactionError> {
    let allowed = fingerprints
        .iter()
        .flat_map(ancestor_paths)
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for ancestor in &intent.created_ancestors {
        KnownGoodRelativePath::new(ancestor)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        if !allowed.contains(ancestor) || !observed.insert(ancestor.clone()) {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    }
    if observed.into_iter().collect::<Vec<_>>() != intent.created_ancestors {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    Ok(())
}

fn ancestor_paths(fingerprint: &EntryFingerprint) -> Vec<String> {
    let mut paths = vec![fingerprint.root.directory_name().to_string()];
    let mut current = fingerprint.root.directory_name().to_string();
    let mut segments = fingerprint.path.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            break;
        }
        current.push('/');
        current.push_str(segment);
        paths.push(current.clone());
    }
    paths
}

fn intent_matches_projection(
    intent: &PersistedIntent,
    version_id: &str,
    activation_contract_id: &ManagedInstallActivationContractId,
    planned: &[PlannedEntry],
) -> Result<bool, VersionBundleTransactionError> {
    let persisted = validate_persisted_intent(intent)?;
    Ok(intent.version_id == version_id
        && intent.activation_contract_id == *activation_contract_id
        && persisted
            == planned
                .iter()
                .map(|entry| entry.fingerprint.clone())
                .collect::<Vec<_>>())
}

fn open_lane(
    lease: &ManagedRootPublicationLease,
) -> Result<ManagedDir, VersionBundleTransactionError> {
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    let publication = lease.publication_directory();
    let lane_existed = publication
        .has_portably_exact_child_name(LANE_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let lane = if lane_existed {
        publication
            .open_child(LANE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    } else {
        publication
            .open_or_create_child(LANE_NAME)
            .map_err(|_| VersionBundleTransactionError::Preparation)?
    };
    lane.sweep_orphan_temps()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let names = exact_names(
        &lane,
        &[
            STAGING_NAME,
            QUARANTINE_NAME,
            INTENT_NAME,
            OUTCOME_NAME,
            SETTLEMENT_NAME,
        ],
        MAX_LANE_ENTRIES,
    )?;
    if !names.contains(INTENT_NAME) && !names.contains(SETTLEMENT_NAME) {
        let clean_reserved = names.len() == 2
            && names.contains(STAGING_NAME)
            && names.contains(QUARANTINE_NAME)
            && lane
                .open_child(STAGING_NAME)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                .entries_bounded(1)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                .is_empty()
            && lane
                .open_child(QUARANTINE_NAME)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                .entries_bounded(1)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
                .is_empty();
        if !names.is_empty() && !clean_reserved {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    Ok(lane)
}

fn version_bundle_effect_boundary(lease: &ManagedRootPublicationLease) -> Option<bool> {
    lease.root().settle().ok()?;
    lease.revalidate().ok()?;
    let publication = lease.publication_directory();
    if !publication.has_portably_exact_child_name(LANE_NAME).ok()? {
        lease.revalidate().ok()?;
        return Some(false);
    }
    let lane = publication.open_child(LANE_NAME).ok()?;
    let mut effect = false;
    for marker in [INTENT_NAME, OUTCOME_NAME, SETTLEMENT_NAME] {
        effect |= lane.inspect_regular_file(marker).ok()?.is_some();
    }
    lease.revalidate().ok()?;
    Some(effect)
}

fn open_or_create_slots_after_intent(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
) -> Result<(ManagedDir, ManagedDir), VersionBundleTransactionError> {
    let staging_exists = lane
        .has_portably_exact_child_name(STAGING_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let quarantine_exists = lane
        .has_portably_exact_child_name(QUARANTINE_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let staging = if staging_exists {
        lane.open_child(STAGING_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    } else {
        lane.open_or_create_child(STAGING_NAME)
            .map_err(|_| VersionBundleTransactionError::Preparation)?
    };
    let quarantine = if quarantine_exists {
        lane.open_child(QUARANTINE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    } else {
        lane.open_or_create_child(QUARANTINE_NAME)
            .map_err(|_| VersionBundleTransactionError::Preparation)?
    };
    staging
        .sweep_orphan_temps()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    quarantine
        .sweep_orphan_temps()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    Ok((staging, quarantine))
}

fn read_intent(
    lane: &ManagedDir,
) -> Result<Option<(PersistedIntent, ManagedFileGuard)>, VersionBundleTransactionError> {
    Ok(read_bounded_marker(lane, INTENT_NAME, MAX_MARKER_BYTES)?)
}

fn read_outcome(
    lane: &ManagedDir,
) -> Result<Option<(PersistedOutcome, ManagedFileGuard)>, VersionBundleTransactionError> {
    Ok(read_bounded_marker(lane, OUTCOME_NAME, MAX_MARKER_BYTES)?)
}

fn read_settlement(
    lane: &ManagedDir,
) -> Result<Option<(PersistedSettlement, ManagedFileGuard)>, VersionBundleTransactionError> {
    Ok(read_bounded_marker(
        lane,
        SETTLEMENT_NAME,
        MAX_MARKER_BYTES,
    )?)
}

fn validate_outcome(
    outcome: &PersistedOutcome,
    intent: &PersistedIntent,
) -> Result<(), VersionBundleTransactionError> {
    if outcome.schema != OUTCOME_SCHEMA || outcome.transaction_nonce != intent.transaction_nonce {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    Ok(())
}

fn validate_settlement(
    settlement: &PersistedSettlement,
) -> Result<(), VersionBundleTransactionError> {
    if settlement.schema != SETTLEMENT_SCHEMA
        || settlement.phase != PersistedSettlementPhase::CallerSettled
        || !valid_nonce(&settlement.generation_nonce)
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    validate_persisted_intent(&settlement.intent)?;
    validate_outcome(&settlement.outcome, &settlement.intent)
}

fn refuse_unacknowledged_settlement(
    lane: &ManagedDir,
) -> Result<(), VersionBundleTransactionError> {
    if read_settlement(lane)?.is_some() {
        return Err(VersionBundleTransactionError::UnacknowledgedSettlement);
    }
    Ok(())
}

struct VersionBundleEvidenceHasher(Sha256);

impl VersionBundleEvidenceHasher {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update(b"\0");
        Self(digest)
    }

    fn finish_hex(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}

impl Hasher for VersionBundleEvidenceHasher {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }
}

pub(crate) fn durable_version_bundle_root_binding(
    root: &ManagedDir,
    transaction_nonce: &str,
    settlement_generation: &str,
) -> Option<String> {
    if !valid_nonce(transaction_nonce) || !valid_nonce(settlement_generation) {
        return None;
    }
    root.settle().ok()?;
    let root_identity = root.identity().ok()?;
    let mut digest = VersionBundleEvidenceHasher::new(b"axial.managed_install_publication.root.v1");
    root_identity.hash_filesystem_binding(&mut digest);
    digest.write(transaction_nonce.as_bytes());
    digest.write(settlement_generation.as_bytes());
    root.revalidate().ok()?;
    Some(digest.finish_hex())
}

fn settlement_fingerprint(
    root_binding: &str,
    settlement: &PersistedSettlement,
) -> Result<String, VersionBundleTransactionError> {
    let bytes = bounded_marker_bytes(settlement, MAX_MARKER_BYTES)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let mut digest =
        VersionBundleEvidenceHasher::new(b"axial.managed_install_publication.evidence.v1");
    digest.write(root_binding.as_bytes());
    digest.write(&bytes);
    Ok(digest.finish_hex())
}

fn durable_classification_from_settlement(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    candidates: &ManagedInstallPublicationCandidates,
    settlement: PersistedSettlement,
    settlement_guard: ManagedFileGuard,
) -> Result<DurableVersionBundleClassification, VersionBundleTransactionError> {
    validate_settlement(&settlement)?;
    if !candidates.contains(&settlement.intent.version_id)
        || !lane
            .file_guard_matches(SETTLEMENT_NAME, &settlement_guard)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    validate_settled_lane_shape(lease, lane, &settlement)?;
    let root_binding = durable_version_bundle_root_binding(
        lease.root(),
        &settlement.intent.transaction_nonce,
        &settlement.generation_nonce,
    )
    .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
    let evidence = DurableVersionBundleEvidence {
        fingerprint: settlement_fingerprint(&root_binding, &settlement)?,
        root_binding,
        settlement_identity: settlement_guard.identity(),
        settlement,
    };
    Ok(match evidence.settlement.outcome.outcome {
        PersistedTerminalOutcome::Committed => {
            DurableVersionBundleClassification::Committed(evidence)
        }
        PersistedTerminalOutcome::RolledBack { effect } => {
            DurableVersionBundleClassification::RolledBack { evidence, effect }
        }
    })
}

fn require_empty_lane(lane: &ManagedDir) -> Result<(), VersionBundleTransactionError> {
    let names = exact_names(lane, &[STAGING_NAME, QUARANTINE_NAME], 2)?;
    if names.is_empty() {
        return Ok(());
    }
    if names.len() != 2
        || !lane
            .open_child(STAGING_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            .entries_bounded(1)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            .is_empty()
        || !lane
            .open_child(QUARANTINE_NAME)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            .entries_bounded(1)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            .is_empty()
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    Ok(())
}

fn validate_existing_portable_paths(
    root: &ManagedDir,
    fingerprints: &[EntryFingerprint],
) -> Result<(), VersionBundleTransactionError> {
    for fingerprint in fingerprints {
        if let Err(error) = crate::managed_publication::validate_existing_managed_target_path(
            root,
            fingerprint.root.directory_name(),
            fingerprint.path.as_str(),
        ) {
            return Err(match error {
                ManagedTargetPathError::PortableAlias => {
                    VersionBundleTransactionError::PortablePathAlias
                }
                ManagedTargetPathError::Access => VersionBundleTransactionError::Preparation,
            });
        }
    }
    Ok(())
}

fn preflight_canonical_targets(
    root: &ManagedDir,
    fingerprints: &[EntryFingerprint],
) -> Result<(Vec<Option<CanonicalTarget>>, Vec<String>), VersionBundleTransactionError> {
    let mut targets = Vec::with_capacity(fingerprints.len());
    let mut created_ancestors = BTreeSet::new();
    let mut displaced_bytes = 0_u64;
    for fingerprint in fingerprints {
        let ancestors = ancestor_paths(fingerprint);
        let Some((parent, name)) = open_canonical_parent(root, fingerprint)? else {
            let mut missing = false;
            for ancestor in ancestors {
                if missing || !managed_directory_path_exists(root, &ancestor)? {
                    missing = true;
                    created_ancestors.insert(ancestor);
                }
            }
            targets.push(None);
            continue;
        };
        let previous = parent
            .inspect_regular_file(&name)
            .map_err(|_| VersionBundleTransactionError::Preparation)?;
        let prior_fingerprint = match previous.as_ref() {
            Some(previous) => {
                if previous.size() > MAX_TIER2_ARTIFACT_BYTES {
                    return Err(VersionBundleTransactionError::Preparation);
                }
                displaced_bytes = displaced_bytes
                    .checked_add(previous.size())
                    .ok_or(VersionBundleTransactionError::Preparation)?;
                if displaced_bytes > MAX_TIER2_AGGREGATE_BYTES {
                    return Err(VersionBundleTransactionError::Preparation);
                }
                PriorFingerprint::ExistingFile {
                    sha1: parent
                        .sha1_guarded_file(&name, previous, MAX_TIER2_ARTIFACT_BYTES)
                        .map_err(|_| VersionBundleTransactionError::Preparation)?,
                    size: previous.size(),
                }
            }
            None => PriorFingerprint::Absent,
        };
        targets.push(Some(CanonicalTarget {
            parent,
            name,
            previous,
            prior_fingerprint,
        }));
    }
    Ok((targets, created_ancestors.into_iter().collect()))
}

fn open_canonical_parent(
    root: &ManagedDir,
    fingerprint: &EntryFingerprint,
) -> Result<Option<(ManagedDir, String)>, VersionBundleTransactionError> {
    Ok(open_managed_target_parent(
        root,
        fingerprint.root.directory_name(),
        fingerprint.path.as_str(),
    )?)
}

fn context_from_prepared(
    handles: TransactionHandles,
    root_identity: ManagedDirectoryIdentity,
    planned: &mut [PlannedEntry],
    #[cfg(any(test, feature = "test-support"))] test_hook: Option<PublicationTestHook>,
) -> Result<TransactionContext, VersionBundleTransactionError> {
    let TransactionHandles {
        lease,
        lane,
        staging,
        quarantine,
        intent,
        intent_guard,
    } = handles;
    validate_slot_topology(&staging, &quarantine, &intent)?;
    if !quarantine
        .entries_bounded(1)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
        .is_empty()
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let targets = rolled_back_targets(lease.root(), &intent)?;
    let mut entries = Vec::with_capacity(planned.len());
    for ((planned, persisted), target) in planned.iter().zip(&intent.entries).zip(targets) {
        let stage_guard = match staging
            .inspect_regular_file(&persisted.staging_slot)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
        {
            Some(guard) => {
                authenticate_guarded_publication_file(
                    &staging,
                    &persisted.staging_slot,
                    &guard,
                    &planned.fingerprint.digest,
                    planned.fingerprint.size,
                    MAX_TIER2_ARTIFACT_BYTES,
                )?;
                guard
            }
            None => {
                staging
                    .write_new_exact(&persisted.staging_slot, planned.source.bytes())
                    .map_err(|_| VersionBundleTransactionError::Preparation)?;
                staging
                    .verify_authenticated(
                        &persisted.staging_slot,
                        planned.fingerprint.size,
                        &planned.fingerprint.digest,
                    )
                    .map_err(|_| VersionBundleTransactionError::Preparation)?;
                staging
                    .inspect_regular_file(&persisted.staging_slot)
                    .map_err(|_| VersionBundleTransactionError::Preparation)?
                    .ok_or(VersionBundleTransactionError::Preparation)?
            }
        };
        entries.push(TransactionEntry {
            fingerprint: planned.fingerprint.clone(),
            stage_name: persisted.staging_slot.clone(),
            quarantine_name: persisted.quarantine_slot.clone(),
            stage_guard: Some(stage_guard),
            canonical_guard: None,
            target,
            state: EntryState::Prepared,
        });
    }
    if !lane
        .file_guard_matches(INTENT_NAME, &intent_guard)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::Preparation)?;
    Ok(TransactionContext {
        lease,
        root_identity,
        lane,
        staging,
        quarantine,
        intent,
        intent_guard,
        outcome_guard: None,
        entries,
        #[cfg(any(test, feature = "test-support"))]
        test_hook,
    })
}

fn validate_slot_topology(
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<(), VersionBundleTransactionError> {
    let stage_names = intent
        .entries
        .iter()
        .map(|entry| entry.staging_slot.as_str())
        .collect::<Vec<_>>();
    let quarantine_names = intent
        .entries
        .iter()
        .map(|entry| entry.quarantine_slot.as_str())
        .collect::<Vec<_>>();
    exact_names(staging, &stage_names, MAX_VERSION_BUNDLE_ENTRIES)?;
    exact_names(quarantine, &quarantine_names, MAX_VERSION_BUNDLE_ENTRIES)?;
    Ok(())
}

fn rolled_back_targets(
    root: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<Vec<Option<CanonicalTarget>>, VersionBundleTransactionError> {
    let fingerprints = validate_persisted_intent(intent)?;
    let mut targets = Vec::with_capacity(fingerprints.len());
    for (fingerprint, persisted) in fingerprints.iter().zip(&intent.entries) {
        let Some((parent, name)) = open_canonical_parent(root, fingerprint)? else {
            if persisted.prior != PriorFingerprint::Absent {
                return Err(VersionBundleTransactionError::RecoveryAmbiguous);
            }
            targets.push(None);
            continue;
        };
        let previous = parent
            .inspect_regular_file(&name)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        match (&persisted.prior, previous.as_ref()) {
            (PriorFingerprint::Absent, None) => {}
            (PriorFingerprint::ExistingFile { sha1, size }, Some(guard)) => {
                authenticate_guarded_publication_file(
                    &parent,
                    &name,
                    guard,
                    sha1,
                    *size,
                    MAX_TIER2_ARTIFACT_BYTES,
                )?;
            }
            _ => return Err(VersionBundleTransactionError::RecoveryAmbiguous),
        }
        targets.push(Some(CanonicalTarget {
            parent,
            name,
            previous,
            prior_fingerprint: persisted.prior.clone(),
        }));
    }
    Ok(targets)
}

fn prepare_canonical_targets(context: &mut TransactionContext) -> Result<(), LoaderError> {
    materialize_recorded_ancestors(context.lease.root(), &context.intent)?;
    for entry in &mut context.entries {
        if entry.target.is_some() {
            continue;
        }
        let Some((parent, name)) =
            open_canonical_parent_loader(context.lease.root(), &entry.fingerprint)?
        else {
            return Err(LoaderError::Verify(
                "version bundle recorded parent was not materialized".to_string(),
            ));
        };
        if parent.inspect_regular_file(&name)?.is_some() {
            return Err(LoaderError::Verify(
                "version bundle target appeared after durable intent".to_string(),
            ));
        }
        entry.target = Some(CanonicalTarget {
            parent,
            name,
            previous: None,
            prior_fingerprint: PriorFingerprint::Absent,
        });
    }
    context.lease.revalidate().map_err(publication_as_loader)
}

fn materialize_recorded_ancestors(
    root: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<(), LoaderError> {
    for relative in &intent.created_ancestors {
        let mut directory = root.clone();
        for segment in relative.split('/') {
            if directory.has_portably_exact_child_name(segment)? {
                directory = directory.open_child(segment)?;
            } else {
                directory = directory.open_or_create_child(segment)?;
            }
        }
    }
    Ok(())
}

fn open_canonical_parent_loader(
    root: &ManagedDir,
    fingerprint: &EntryFingerprint,
) -> Result<Option<(ManagedDir, String)>, LoaderError> {
    open_managed_target_parent(
        root,
        fingerprint.root.directory_name(),
        fingerprint.path.as_str(),
    )
    .map_err(|_| LoaderError::Verify("version bundle target path changed".to_string()))
}

enum ObservedCanonical {
    Absent,
    Source(ManagedFileGuard),
    Prior(ManagedFileGuard),
}

impl ObservedCanonical {
    fn state(&self) -> ManagedCanonicalState {
        match self {
            Self::Absent => ManagedCanonicalState::Absent,
            Self::Source(_) => ManagedCanonicalState::Source,
            Self::Prior(_) => ManagedCanonicalState::Prior,
        }
    }
}

struct RecoveryObservation {
    parent: Option<ManagedDir>,
    name: String,
    canonical: ObservedCanonical,
    stage: Option<ManagedFileGuard>,
    quarantine: Option<ManagedFileGuard>,
}

fn observe_recovery_entry(
    root: &ManagedDir,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    fingerprint: &EntryFingerprint,
    persisted: &PersistedEntry,
) -> Result<RecoveryObservation, VersionBundleTransactionError> {
    let (parent, name, canonical_guard) = match open_canonical_parent(root, fingerprint)? {
        Some((parent, name)) => {
            let guard = parent
                .inspect_regular_file(&name)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
            (Some(parent), name, guard)
        }
        None => (
            None,
            fingerprint
                .path
                .as_str()
                .rsplit('/')
                .next()
                .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?
                .to_string(),
            None,
        ),
    };
    let canonical = match (parent.as_ref(), canonical_guard) {
        (_, None) => ObservedCanonical::Absent,
        (Some(parent), Some(guard)) => {
            let digest = parent
                .sha1_guarded_file(&name, &guard, MAX_TIER2_ARTIFACT_BYTES)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
            if guard.size() == fingerprint.size && digest == fingerprint.digest {
                ObservedCanonical::Source(guard)
            } else if matches!(
                &persisted.prior,
                PriorFingerprint::ExistingFile { sha1, size }
                    if *size == guard.size() && sha1 == &digest
            ) {
                ObservedCanonical::Prior(guard)
            } else {
                return Err(VersionBundleTransactionError::RecoveryAmbiguous);
            }
        }
        (None, Some(_)) => return Err(VersionBundleTransactionError::RecoveryAmbiguous),
    };
    let stage = staging
        .inspect_regular_file(&persisted.staging_slot)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    if let Some(stage) = stage.as_ref() {
        authenticate_guarded_publication_file(
            staging,
            &persisted.staging_slot,
            stage,
            &fingerprint.digest,
            fingerprint.size,
            MAX_TIER2_ARTIFACT_BYTES,
        )?;
    }
    let quarantined = quarantine
        .inspect_regular_file(&persisted.quarantine_slot)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    match (&persisted.prior, quarantined.as_ref()) {
        (_, None) => {}
        (PriorFingerprint::ExistingFile { sha1, size }, Some(guard)) => {
            authenticate_guarded_publication_file(
                quarantine,
                &persisted.quarantine_slot,
                guard,
                sha1,
                *size,
                MAX_TIER2_ARTIFACT_BYTES,
            )?;
        }
        (PriorFingerprint::Absent, Some(_)) => {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    }
    Ok(RecoveryObservation {
        parent,
        name,
        canonical,
        stage,
        quarantine: quarantined,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnfinishedMoveOutcome {
    Committed,
    RolledBack,
}

fn reconcile_unfinished_moves(
    lease: &ManagedRootPublicationLease,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<UnfinishedMoveOutcome, VersionBundleTransactionError> {
    validate_slot_topology(staging, quarantine, intent)?;
    let fingerprints = validate_persisted_intent(intent)?;
    let mut observations = fingerprints
        .iter()
        .zip(&intent.entries)
        .map(|(fingerprint, persisted)| {
            observe_recovery_entry(lease.root(), staging, quarantine, fingerprint, persisted)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if observations
        .iter()
        .zip(fingerprints.iter().zip(&intent.entries))
        .all(|(observed, (fingerprint, persisted))| {
            committed_terminal_shape_is_valid(
                &persisted.prior,
                &fingerprint.digest,
                fingerprint.size,
                observed.canonical.state(),
                observed.stage.is_some(),
                observed.quarantine.is_some(),
            )
        })
    {
        return Ok(UnfinishedMoveOutcome::Committed);
    }
    if !observations
        .iter()
        .zip(fingerprints.iter().zip(&intent.entries))
        .all(|(observed, (fingerprint, persisted))| {
            rollback_terminal_shape_is_reachable(
                &persisted.prior,
                &fingerprint.digest,
                fingerprint.size,
                observed.canonical.state(),
                observed.stage.is_some(),
                observed.quarantine.is_some(),
            )
        })
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }

    for index in (0..observations.len()).rev() {
        let observed = &mut observations[index];
        let RecoveryObservation {
            parent,
            name,
            canonical,
            stage,
            quarantine: observed_quarantine,
        } = observed;
        let persisted = &intent.entries[index];
        let fingerprint = &fingerprints[index];
        match &persisted.prior {
            PriorFingerprint::Absent => {
                if observed_quarantine.is_some() {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                match canonical {
                    ObservedCanonical::Source(source) => {
                        if stage.is_some() {
                            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                        }
                        let parent = parent
                            .as_ref()
                            .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                        parent
                            .rename_guarded_file_no_replace(
                                name,
                                source,
                                staging,
                                &persisted.staging_slot,
                            )
                            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
                    }
                    ObservedCanonical::Absent => {}
                    ObservedCanonical::Prior(_) => {
                        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                    }
                }
            }
            PriorFingerprint::ExistingFile { sha1, size }
                if persisted
                    .prior
                    .matches_source(&fingerprint.digest, fingerprint.size) =>
            {
                if observed_quarantine.is_some()
                    || !matches!(canonical, ObservedCanonical::Source(_))
                {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                let parent = parent
                    .as_ref()
                    .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                let ObservedCanonical::Source(guard) = canonical else {
                    unreachable!("matched source state")
                };
                authenticate_guarded_publication_file(
                    parent,
                    name,
                    guard,
                    sha1,
                    *size,
                    MAX_TIER2_ARTIFACT_BYTES,
                )?;
            }
            PriorFingerprint::ExistingFile { .. } => match canonical {
                ObservedCanonical::Source(source) => {
                    if stage.is_some() {
                        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                    }
                    let prior = observed_quarantine
                        .as_mut()
                        .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                    let parent = parent
                        .as_ref()
                        .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                    parent
                        .rename_guarded_file_no_replace(
                            name,
                            source,
                            staging,
                            &persisted.staging_slot,
                        )
                        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
                    quarantine
                        .rename_guarded_file_no_replace(
                            &persisted.quarantine_slot,
                            prior,
                            parent,
                            name,
                        )
                        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
                }
                ObservedCanonical::Absent => {
                    let prior = observed_quarantine
                        .as_mut()
                        .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                    let parent = parent
                        .as_ref()
                        .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                    quarantine
                        .rename_guarded_file_no_replace(
                            &persisted.quarantine_slot,
                            prior,
                            parent,
                            name,
                        )
                        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
                }
                ObservedCanonical::Prior(_) => {
                    if observed_quarantine.is_some() {
                        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                    }
                }
            },
        }
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    Ok(UnfinishedMoveOutcome::RolledBack)
}

fn recover_unfinished_commit(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
    planned: &[PlannedEntry],
) -> Result<Option<ManagedFileGuard>, VersionBundleTransactionError> {
    if reconcile_unfinished_moves(lease, staging, quarantine, intent)?
        == UnfinishedMoveOutcome::Committed
    {
        return match write_outcome(lane, intent, PersistedTerminalOutcome::Committed) {
            Ok(guard) => Ok(Some(guard)),
            Err(OutcomeWriteFailure::BeforePromotion) => {
                Err(VersionBundleTransactionError::Preparation)
            }
            Err(OutcomeWriteFailure::PromotionAttempted(Some(guard))) => {
                lease
                    .root()
                    .settle()
                    .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?;
                lease
                    .revalidate()
                    .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?;
                if !lane
                    .file_guard_matches(OUTCOME_NAME, &guard)
                    .map_err(|_| VersionBundleTransactionError::RecoveryUnsettled)?
                {
                    return Err(VersionBundleTransactionError::RecoveryUnsettled);
                }
                Ok(Some(guard))
            }
            Err(OutcomeWriteFailure::PromotionAttempted(None)) => {
                Err(VersionBundleTransactionError::RecoveryUnsettled)
            }
        };
    }
    // Preparation can have stopped after intent but before every stage write. The
    // retry supplies the same authenticated projection and completes only missing slots.
    for (planned, persisted) in planned.iter().zip(&intent.entries) {
        if staging
            .inspect_regular_file(&persisted.staging_slot)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
            .is_none()
        {
            staging
                .write_new_exact(&persisted.staging_slot, planned.source.bytes())
                .map_err(|_| VersionBundleTransactionError::Preparation)?;
        }
        staging
            .verify_authenticated(
                &persisted.staging_slot,
                planned.fingerprint.size,
                &planned.fingerprint.digest,
            )
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    Ok(None)
}

enum OutcomeWriteFailure {
    BeforePromotion,
    PromotionAttempted(Option<ManagedFileGuard>),
}

fn write_outcome(
    lane: &ManagedDir,
    intent: &PersistedIntent,
    outcome: PersistedTerminalOutcome,
) -> Result<ManagedFileGuard, OutcomeWriteFailure> {
    let marker = PersistedOutcome {
        schema: OUTCOME_SCHEMA.to_string(),
        transaction_nonce: intent.transaction_nonce.clone(),
        outcome,
    };
    let bytes = bounded_marker_bytes(&marker, MAX_MARKER_BYTES)
        .map_err(|_| OutcomeWriteFailure::BeforePromotion)?;
    match lane.write_new_exact_retained(OUTCOME_NAME, &bytes) {
        Ok(guard) => Ok(guard),
        Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_)) => {
            Err(OutcomeWriteFailure::BeforePromotion)
        }
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
            Err(OutcomeWriteFailure::PromotionAttempted(final_guard))
        }
    }
}

fn write_settlement(
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
    write_settlement_inner(
        lane,
        settlement,
        #[cfg(test)]
        None,
    )
}

fn write_settlement_inner(
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
    #[cfg(test)] fault: Option<ManagedCreateOnlyWriteFault>,
) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
    let bytes = bounded_marker_bytes(settlement, MAX_MARKER_BYTES).map_err(|_| {
        ManagedCreateOnlyWriteFailure::BeforePromotion(LoaderError::Verify(
            "version bundle settlement marker is invalid".to_string(),
        ))
    })?;
    #[cfg(test)]
    let write = match fault {
        Some(fault) => lane.write_new_exact_retained_with_fault(SETTLEMENT_NAME, &bytes, fault),
        None => lane.write_new_exact_retained(SETTLEMENT_NAME, &bytes),
    };
    #[cfg(not(test))]
    let write = lane.write_new_exact_retained(SETTLEMENT_NAME, &bytes);
    match write {
        Ok(guard) => Ok(guard),
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
            final_guard: Some(guard),
        }) => {
            if lane.sync().is_err() {
                return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                    final_guard: Some(guard),
                });
            }
            let observed = read_settlement(lane).map_err(|_| {
                ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard: None }
            })?;
            if matches!(
                observed,
                Some((ref marker, ref observed_guard))
                    if marker == settlement && observed_guard.identity() == guard.identity()
            ) && lane
                .file_guard_matches(SETTLEMENT_NAME, &guard)
                .is_ok_and(|matches| matches)
                && lane.revalidate().is_ok()
            {
                Ok(guard)
            } else {
                Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                    final_guard: Some(guard),
                })
            }
        }
        Err(error) => Err(error),
    }
}

fn reconstruct_terminal_context(
    handles: TransactionHandles,
    outcome: PersistedOutcome,
    outcome_guard: ManagedFileGuard,
    #[cfg(any(test, feature = "test-support"))] test_hook: Option<PublicationTestHook>,
) -> Result<TransactionContext, VersionBundleTransactionError> {
    let TransactionHandles {
        lease,
        lane,
        staging,
        quarantine,
        intent,
        intent_guard,
    } = handles;
    if !lane
        .file_guard_matches(OUTCOME_NAME, &outcome_guard)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    validate_outcome(&outcome, &intent)?;
    validate_slot_topology(&staging, &quarantine, &intent)?;
    let fingerprints = validate_persisted_intent(&intent)?;
    let observations = fingerprints
        .iter()
        .zip(&intent.entries)
        .map(|(fingerprint, persisted)| {
            observe_recovery_entry(lease.root(), &staging, &quarantine, fingerprint, persisted)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut entries = Vec::with_capacity(fingerprints.len());
    for ((fingerprint, persisted), observed) in fingerprints
        .into_iter()
        .zip(&intent.entries)
        .zip(observations)
    {
        let RecoveryObservation {
            parent,
            name,
            canonical,
            stage,
            quarantine: quarantined,
        } = observed;
        let (state, target, canonical_guard, stage_guard) = match outcome.outcome {
            PersistedTerminalOutcome::Committed => {
                if !committed_terminal_shape_is_valid(
                    &persisted.prior,
                    &fingerprint.digest,
                    fingerprint.size,
                    canonical.state(),
                    stage.is_some(),
                    quarantined.is_some(),
                ) {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                // Re-observe after the shape check because guards are intentionally non-cloneable.
                let observed = observe_recovery_entry(
                    lease.root(),
                    &staging,
                    &quarantine,
                    &fingerprint,
                    persisted,
                )?;
                let RecoveryObservation {
                    parent,
                    name,
                    canonical,
                    stage,
                    quarantine: quarantined,
                } = observed;
                let ObservedCanonical::Source(canonical_guard) = canonical else {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                };
                let parent = parent.ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?;
                let (state, previous) = match &persisted.prior {
                    PriorFingerprint::Absent => (EntryState::PublishedNew, None),
                    PriorFingerprint::ExistingFile { .. }
                        if persisted
                            .prior
                            .matches_source(&fingerprint.digest, fingerprint.size) =>
                    {
                        (EntryState::AlreadyExact, None)
                    }
                    PriorFingerprint::ExistingFile { .. } => (
                        EntryState::PublishedReplacement,
                        Some(quarantined.ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?),
                    ),
                };
                (
                    state,
                    Some(CanonicalTarget {
                        parent,
                        name,
                        previous,
                        prior_fingerprint: persisted.prior.clone(),
                    }),
                    Some(canonical_guard),
                    stage,
                )
            }
            PersistedTerminalOutcome::RolledBack { .. } => {
                if quarantined.is_some() || stage.is_none() {
                    return Err(VersionBundleTransactionError::RecoveryAmbiguous);
                }
                let target = match (&persisted.prior, canonical) {
                    (PriorFingerprint::Absent, ObservedCanonical::Absent) => {
                        parent.map(|parent| CanonicalTarget {
                            parent,
                            name,
                            previous: None,
                            prior_fingerprint: PriorFingerprint::Absent,
                        })
                    }
                    (PriorFingerprint::ExistingFile { .. }, ObservedCanonical::Prior(guard)) => {
                        Some(CanonicalTarget {
                            parent: parent
                                .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?,
                            name,
                            previous: Some(guard),
                            prior_fingerprint: persisted.prior.clone(),
                        })
                    }
                    (PriorFingerprint::ExistingFile { .. }, ObservedCanonical::Source(guard))
                        if persisted
                            .prior
                            .matches_source(&fingerprint.digest, fingerprint.size) =>
                    {
                        Some(CanonicalTarget {
                            parent: parent
                                .ok_or(VersionBundleTransactionError::RecoveryAmbiguous)?,
                            name,
                            previous: Some(guard),
                            prior_fingerprint: persisted.prior.clone(),
                        })
                    }
                    _ => return Err(VersionBundleTransactionError::RecoveryAmbiguous),
                };
                (EntryState::RolledBack, target, None, stage)
            }
        };
        entries.push(TransactionEntry {
            fingerprint,
            stage_name: persisted.staging_slot.clone(),
            quarantine_name: persisted.quarantine_slot.clone(),
            stage_guard,
            canonical_guard,
            target,
            state,
        });
    }
    let root_identity = lease
        .root()
        .identity()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let context = TransactionContext {
        lease,
        root_identity,
        lane,
        staging,
        quarantine,
        intent,
        intent_guard,
        outcome_guard: Some(outcome_guard),
        entries,
        #[cfg(any(test, feature = "test-support"))]
        test_hook,
    };
    match outcome.outcome {
        PersistedTerminalOutcome::Committed => revalidate_committed(&context),
        PersistedTerminalOutcome::RolledBack { .. } => revalidate_failure(&context),
    }
    .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    Ok(context)
}

fn committed_receipt(context: TransactionContext) -> VersionBundleTransactionCommitReceipt {
    VersionBundleTransactionCommitReceipt {
        context: Arc::new(context),
    }
}

enum MutationDecision {
    Committed,
    RolledBack {
        effect: VersionBundleTransactionEffect,
    },
    Pending {
        effect: VersionBundleTransactionEffect,
    },
}

enum EntryPromotionFailure {
    BeforeEffect,
    EffectUnsettled,
}

impl From<LoaderError> for EntryPromotionFailure {
    fn from(_error: LoaderError) -> Self {
        Self::BeforeEffect
    }
}

impl From<ManagedGuardedFileMoveFailure> for EntryPromotionFailure {
    fn from(failure: ManagedGuardedFileMoveFailure) -> Self {
        match failure {
            ManagedGuardedFileMoveFailure::NoEffect => Self::BeforeEffect,
            ManagedGuardedFileMoveFailure::AppliedUnsettled
            | ManagedGuardedFileMoveFailure::Indeterminate => Self::EffectUnsettled,
        }
    }
}

async fn mutate_owned_context(
    context: TransactionContext,
) -> Result<VersionBundleTransactionCommitReceipt, VersionBundleTransactionFailureReceipt> {
    let holder = Arc::new(Mutex::new(Some(context)));
    let worker_holder = Arc::clone(&holder);
    let attempted = run_publication_blocking(move || {
        let mut slot = worker_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = slot
            .as_mut()
            .expect("version bundle mutation context is retained");
        let mut current_effect = VersionBundleTransactionEffect::Promotion;
        let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mutate_in_place(context, &mut current_effect)
        }))
        .unwrap_or(MutationDecision::Pending {
            effect: current_effect,
        });
        (decision, current_effect)
    })
    .await;
    let context = holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("version bundle mutation worker retained its context");
    match attempted {
        Ok((MutationDecision::Committed, _)) => Ok(VersionBundleTransactionCommitReceipt {
            context: Arc::new(context),
        }),
        Ok((MutationDecision::RolledBack { effect }, _)) => Err(terminal_failure(context, effect)),
        Ok((MutationDecision::Pending { effect }, _)) => {
            Err(reconciliation_failure(context, effect))
        }
        Err(_) => Err(reconciliation_failure(
            context,
            VersionBundleTransactionEffect::Promotion,
        )),
    }
}

fn mutate_in_place(
    context: &mut TransactionContext,
    current_effect: &mut VersionBundleTransactionEffect,
) -> MutationDecision {
    *current_effect = VersionBundleTransactionEffect::Promotion;
    if prepare_canonical_targets(context).is_err() {
        return rollback_mutation_failure(
            context,
            VersionBundleTransactionEffect::Promotion,
            current_effect,
        );
    }
    for index in 0..context.entries.len() {
        if context.lease.revalidate().is_err() {
            return rollback_mutation_failure(
                context,
                VersionBundleTransactionEffect::Promotion,
                current_effect,
            );
        }
        match promote_entry(context, index) {
            Ok(()) => {}
            Err(EntryPromotionFailure::BeforeEffect) => {
                return rollback_mutation_failure(
                    context,
                    VersionBundleTransactionEffect::Promotion,
                    current_effect,
                );
            }
            Err(EntryPromotionFailure::EffectUnsettled) => {
                return MutationDecision::Pending {
                    effect: VersionBundleTransactionEffect::Promotion,
                };
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        if apply_test_hook(context, index + 1) {
            return rollback_mutation_failure(
                context,
                VersionBundleTransactionEffect::Promotion,
                current_effect,
            );
        }
    }
    *current_effect = VersionBundleTransactionEffect::Postcheck;
    if verify_committed_physical(context).is_err() {
        return rollback_mutation_failure(
            context,
            VersionBundleTransactionEffect::Postcheck,
            current_effect,
        );
    }
    let outcome_guard = match write_outcome(
        &context.lane,
        &context.intent,
        PersistedTerminalOutcome::Committed,
    ) {
        Ok(guard) => guard,
        Err(OutcomeWriteFailure::BeforePromotion) => {
            return MutationDecision::Pending {
                effect: VersionBundleTransactionEffect::Postcheck,
            };
        }
        Err(OutcomeWriteFailure::PromotionAttempted(final_guard)) => {
            context.outcome_guard = final_guard;
            return MutationDecision::Pending {
                effect: VersionBundleTransactionEffect::Postcheck,
            };
        }
    };
    context.outcome_guard = Some(outcome_guard);
    #[cfg(test)]
    if matches!(
        context.test_hook.as_ref(),
        Some(PublicationTestHook::FailAfterCommittedOutcomeOnce)
    ) {
        context.test_hook = None;
        return MutationDecision::Pending {
            effect: VersionBundleTransactionEffect::Postcheck,
        };
    }
    if revalidate_committed(context).is_err() {
        return MutationDecision::Pending {
            effect: VersionBundleTransactionEffect::Postcheck,
        };
    }
    MutationDecision::Committed
}

fn rollback_mutation_failure(
    context: &mut TransactionContext,
    effect: VersionBundleTransactionEffect,
    current_effect: &mut VersionBundleTransactionEffect,
) -> MutationDecision {
    *current_effect = VersionBundleTransactionEffect::Rollback;
    if rollback(context).is_ok() {
        match write_outcome(
            &context.lane,
            &context.intent,
            PersistedTerminalOutcome::RolledBack { effect },
        ) {
            Ok(guard) => context.outcome_guard = Some(guard),
            Err(OutcomeWriteFailure::BeforePromotion) => {
                return MutationDecision::Pending {
                    effect: VersionBundleTransactionEffect::Rollback,
                };
            }
            Err(OutcomeWriteFailure::PromotionAttempted(final_guard)) => {
                context.outcome_guard = final_guard;
                return MutationDecision::Pending {
                    effect: VersionBundleTransactionEffect::Rollback,
                };
            }
        }
        if revalidate_failure(context).is_ok() {
            return MutationDecision::RolledBack { effect };
        }
    }
    MutationDecision::Pending {
        effect: VersionBundleTransactionEffect::Rollback,
    }
}

fn promote_entry(
    context: &mut TransactionContext,
    index: usize,
) -> Result<(), EntryPromotionFailure> {
    let entry = &mut context.entries[index];
    let target = entry
        .target
        .as_mut()
        .ok_or_else(|| LoaderError::Verify("version bundle target was not prepared".to_string()))?;
    if target
        .prior_fingerprint
        .matches_source(&entry.fingerprint.digest, entry.fingerprint.size)
    {
        let previous = target.previous.as_ref().ok_or_else(|| {
            LoaderError::Verify("version bundle exact prior guard is absent".to_string())
        })?;
        let PriorFingerprint::ExistingFile { sha1, size } = &target.prior_fingerprint else {
            unreachable!("exact prior fingerprint")
        };
        if previous.size() != *size
            || target
                .parent
                .sha1_guarded_file(&target.name, previous, MAX_TIER2_ARTIFACT_BYTES)?
                != *sha1
        {
            return Err(LoaderError::Verify(
                "version bundle exact prior changed before publication".to_string(),
            )
            .into());
        }
        entry.canonical_guard = target.parent.inspect_regular_file(&target.name)?;
        entry.state = EntryState::AlreadyExact;
        return Ok(());
    }
    if let Some(previous) = target.previous.as_ref() {
        let PriorFingerprint::ExistingFile { sha1, size } = &target.prior_fingerprint else {
            return Err(LoaderError::Verify(
                "version bundle prior guard lacks a fingerprint".to_string(),
            )
            .into());
        };
        if previous.size() != *size
            || target
                .parent
                .sha1_guarded_file(&target.name, previous, MAX_TIER2_ARTIFACT_BYTES)?
                != *sha1
        {
            return Err(LoaderError::Verify(
                "version bundle prior changed before quarantine".to_string(),
            )
            .into());
        }
        let previous = target
            .previous
            .as_mut()
            .expect("validated prior guard remains retained");
        target.parent.rename_guarded_file_no_replace(
            &target.name,
            previous,
            &context.quarantine,
            &entry.quarantine_name,
        )?;
        entry.state = EntryState::Quarantined;
        if context.lease.revalidate().is_err() {
            return Err(EntryPromotionFailure::EffectUnsettled);
        }
        let prior_is_exact = previous.size() == *size
            && context
                .quarantine
                .sha1_guarded_file(&entry.quarantine_name, previous, MAX_TIER2_ARTIFACT_BYTES)
                .is_ok_and(|observed| observed == *sha1);
        if !prior_is_exact {
            return Err(EntryPromotionFailure::EffectUnsettled);
        }
    }
    let stage_guard = entry.stage_guard.as_mut().ok_or_else(|| {
        LoaderError::Verify("version bundle staged source guard is absent".to_string())
    })?;
    context.staging.rename_guarded_file_no_replace(
        &entry.stage_name,
        stage_guard,
        &target.parent,
        &target.name,
    )?;
    #[cfg(test)]
    if take_report_first_move_unsettled(&mut context.test_hook) {
        return Err(EntryPromotionFailure::EffectUnsettled);
    }
    entry.canonical_guard = entry.stage_guard.take();
    entry.state = if target.previous.is_some() {
        EntryState::PublishedReplacement
    } else {
        EntryState::PublishedNew
    };
    if context.lease.revalidate().is_err()
        || target
            .parent
            .verify_authenticated(
                &target.name,
                entry.fingerprint.size,
                &entry.fingerprint.digest,
            )
            .is_err()
    {
        return Err(EntryPromotionFailure::EffectUnsettled);
    }
    Ok(())
}

fn rollback(context: &mut TransactionContext) -> Result<(), ()> {
    let mut complete = true;
    for entry in context.entries.iter_mut().rev() {
        let Some(target) = entry.target.as_mut() else {
            if entry.state != EntryState::Prepared {
                entry.state = EntryState::RollbackUncertain;
                complete = false;
            } else {
                entry.state = EntryState::RolledBack;
            }
            continue;
        };
        if matches!(
            entry.state,
            EntryState::PublishedNew | EntryState::PublishedReplacement
        ) {
            let Some(canonical_guard) = entry.canonical_guard.as_mut() else {
                entry.state = EntryState::RollbackUncertain;
                complete = false;
                continue;
            };
            if target
                .parent
                .rename_guarded_file_no_replace(
                    &target.name,
                    canonical_guard,
                    &context.staging,
                    &entry.stage_name,
                )
                .is_err()
            {
                entry.state = EntryState::RollbackUncertain;
                complete = false;
                continue;
            }
            entry.stage_guard = entry.canonical_guard.take();
        }
        if matches!(
            entry.state,
            EntryState::Quarantined | EntryState::PublishedReplacement
        ) {
            let Some(previous) = target.previous.as_mut() else {
                entry.state = EntryState::RollbackUncertain;
                complete = false;
                continue;
            };
            if context
                .quarantine
                .rename_guarded_file_no_replace(
                    &entry.quarantine_name,
                    previous,
                    &target.parent,
                    &target.name,
                )
                .is_err()
            {
                entry.state = EntryState::RollbackUncertain;
                complete = false;
                continue;
            }
        }
        entry.state = EntryState::RolledBack;
    }
    context.lease.revalidate().map_err(|_| ())?;
    complete.then_some(()).ok_or(())
}

fn verify_committed_physical(context: &TransactionContext) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(INTENT_NAME, &context.intent_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle committed intent identity changed".to_string(),
        ));
    }
    for (entry, persisted) in context.entries.iter().zip(&context.intent.entries) {
        let target = entry.target.as_ref().ok_or_else(|| {
            LoaderError::Verify("version bundle committed target is absent".to_string())
        })?;
        let canonical_guard = entry.canonical_guard.as_ref().ok_or_else(|| {
            LoaderError::Verify("version bundle committed canonical guard is absent".to_string())
        })?;
        if !target
            .parent
            .file_guard_matches(&target.name, canonical_guard)?
        {
            return Err(LoaderError::Verify(
                "version bundle committed canonical identity changed".to_string(),
            ));
        }
        target.parent.verify_authenticated(
            &target.name,
            entry.fingerprint.size,
            &entry.fingerprint.digest,
        )?;
        match entry.state {
            EntryState::AlreadyExact => {
                let stage = entry.stage_guard.as_ref().ok_or_else(|| {
                    LoaderError::Verify(
                        "version bundle already-exact stage guard is absent".to_string(),
                    )
                })?;
                if !context
                    .staging
                    .file_guard_matches(&entry.stage_name, stage)?
                {
                    return Err(LoaderError::Verify(
                        "version bundle already-exact stage identity changed".to_string(),
                    ));
                }
                context.staging.verify_authenticated(
                    &entry.stage_name,
                    entry.fingerprint.size,
                    &entry.fingerprint.digest,
                )?;
            }
            EntryState::PublishedNew => {
                if entry.stage_guard.is_some()
                    || context
                        .staging
                        .inspect_regular_file(&entry.stage_name)?
                        .is_some()
                    || context
                        .quarantine
                        .inspect_regular_file(&entry.quarantine_name)?
                        .is_some()
                {
                    return Err(LoaderError::Verify(
                        "version bundle new publication retained an unexpected slot".to_string(),
                    ));
                }
            }
            EntryState::PublishedReplacement => {
                if entry.stage_guard.is_some()
                    || context
                        .staging
                        .inspect_regular_file(&entry.stage_name)?
                        .is_some()
                {
                    return Err(LoaderError::Verify(
                        "version bundle replacement retained its stage".to_string(),
                    ));
                }
                let previous = target.previous.as_ref().ok_or_else(|| {
                    LoaderError::Verify(
                        "version bundle replacement prior guard is absent".to_string(),
                    )
                })?;
                if !context
                    .quarantine
                    .file_guard_matches(&entry.quarantine_name, previous)?
                {
                    return Err(LoaderError::Verify(
                        "version bundle replacement quarantine identity changed".to_string(),
                    ));
                }
                let PriorFingerprint::ExistingFile { sha1, size } = &persisted.prior else {
                    return Err(LoaderError::Verify(
                        "version bundle replacement prior fingerprint is absent".to_string(),
                    ));
                };
                if context.quarantine.sha1_guarded_file(
                    &entry.quarantine_name,
                    previous,
                    MAX_TIER2_ARTIFACT_BYTES,
                )? != *sha1
                    || previous.size() != *size
                {
                    return Err(LoaderError::Verify(
                        "version bundle replacement quarantine changed".to_string(),
                    ));
                }
            }
            EntryState::Prepared
            | EntryState::Quarantined
            | EntryState::RolledBack
            | EntryState::RollbackUncertain => {
                return Err(LoaderError::Verify(
                    "version bundle committed receipt state is invalid".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn revalidate_committed(context: &TransactionContext) -> Result<(), LoaderError> {
    verify_committed_physical(context)?;
    let outcome_guard = context.outcome_guard.as_ref().ok_or_else(|| {
        LoaderError::Verify("version bundle committed outcome guard is absent".to_string())
    })?;
    if !context
        .lane
        .file_guard_matches(OUTCOME_NAME, outcome_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle committed outcome identity changed".to_string(),
        ));
    }
    let (outcome, observed) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| LoaderError::Verify("version bundle outcome is absent".to_string()))?;
    if observed.identity() != outcome_guard.identity()
        || !context.lane.file_guard_matches(OUTCOME_NAME, &observed)?
        || validate_outcome(&outcome, &context.intent).is_err()
        || outcome.outcome != PersistedTerminalOutcome::Committed
    {
        return Err(LoaderError::Verify(
            "version bundle committed outcome changed".to_string(),
        ));
    }
    Ok(())
}

fn revalidate_failure(context: &TransactionContext) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(INTENT_NAME, &context.intent_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle rollback intent identity changed".to_string(),
        ));
    }
    let outcome_guard = context.outcome_guard.as_ref().ok_or_else(|| {
        LoaderError::Verify("version bundle rollback outcome guard is absent".to_string())
    })?;
    if !context
        .lane
        .file_guard_matches(OUTCOME_NAME, outcome_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle rollback outcome identity changed".to_string(),
        ));
    }
    let (outcome, observed) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle rollback outcome is absent".to_string())
        })?;
    if observed.identity() != outcome_guard.identity()
        || !context.lane.file_guard_matches(OUTCOME_NAME, &observed)?
        || validate_outcome(&outcome, &context.intent).is_err()
        || !matches!(outcome.outcome, PersistedTerminalOutcome::RolledBack { .. })
    {
        return Err(LoaderError::Verify(
            "version bundle rollback outcome changed".to_string(),
        ));
    }
    for (entry, persisted) in context.entries.iter().zip(&context.intent.entries) {
        if entry.state != EntryState::RolledBack || entry.canonical_guard.is_some() {
            return Err(LoaderError::Verify(
                "version bundle rollback receipt state is invalid".to_string(),
            ));
        }
        let stage = entry.stage_guard.as_ref().ok_or_else(|| {
            LoaderError::Verify("version bundle rollback stage guard is absent".to_string())
        })?;
        if !context
            .staging
            .file_guard_matches(&entry.stage_name, stage)?
        {
            return Err(LoaderError::Verify(
                "version bundle rollback stage identity changed".to_string(),
            ));
        }
        context.staging.verify_authenticated(
            &entry.stage_name,
            entry.fingerprint.size,
            &entry.fingerprint.digest,
        )?;
        if context
            .quarantine
            .inspect_regular_file(&entry.quarantine_name)?
            .is_some()
        {
            return Err(LoaderError::Verify(
                "version bundle rollback retained quarantine".to_string(),
            ));
        }
        match (&persisted.prior, entry.target.as_ref()) {
            (PriorFingerprint::Absent, Some(target)) => {
                if target.parent.inspect_regular_file(&target.name)?.is_some() {
                    return Err(LoaderError::Verify(
                        "version bundle rollback new target is not absent".to_string(),
                    ));
                }
            }
            (PriorFingerprint::Absent, None) => {
                if open_canonical_parent_loader(context.lease.root(), &entry.fingerprint)?
                    .is_some_and(|(parent, name)| {
                        parent.inspect_regular_file(&name).ok().flatten().is_some()
                    })
                {
                    return Err(LoaderError::Verify(
                        "version bundle rollback absent target appeared".to_string(),
                    ));
                }
            }
            (PriorFingerprint::ExistingFile { sha1, size }, Some(target)) => {
                let previous = target.previous.as_ref().ok_or_else(|| {
                    LoaderError::Verify("version bundle rollback prior guard is absent".to_string())
                })?;
                if !target.parent.file_guard_matches(&target.name, previous)?
                    || previous.size() != *size
                    || target.parent.sha1_guarded_file(
                        &target.name,
                        previous,
                        MAX_TIER2_ARTIFACT_BYTES,
                    )? != *sha1
                {
                    return Err(LoaderError::Verify(
                        "version bundle rollback prior changed".to_string(),
                    ));
                }
            }
            (PriorFingerprint::ExistingFile { .. }, None) => {
                return Err(LoaderError::Verify(
                    "version bundle rollback prior target is absent".to_string(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn apply_test_hook(context: &mut TransactionContext, promotions: usize) -> bool {
    #[cfg(test)]
    let promoted_kind = promotions
        .checked_sub(1)
        .and_then(|index| context.entries.get(index))
        .map(|entry| entry.fingerprint.kind);
    match context.test_hook.as_mut() {
        Some(PublicationTestHook::FailAfter {
            promotions: expected,
        }) if *expected == promotions => true,
        #[cfg(test)]
        Some(PublicationTestHook::PauseAfter {
            promotions: expected,
            reached,
            release,
        }) if *expected == promotions => {
            if let Some(reached) = reached.take() {
                let _ = reached.send(());
            }
            if let Some(release) = release.take() {
                let _ = release.blocking_recv();
            }
            false
        }
        #[cfg(test)]
        Some(PublicationTestHook::CrashAfterPromotion { kind }) if Some(*kind) == promoted_kind => {
            panic!("injected version bundle crash after promotion");
        }
        Some(PublicationTestHook::FailAfter { .. }) | None => false,
        #[cfg(test)]
        Some(
            PublicationTestHook::PauseAfter { .. }
            | PublicationTestHook::CrashAfterPromotion { .. }
            | PublicationTestHook::ReportFirstMoveUnsettled
            | PublicationTestHook::IntentWriteFault(_)
            | PublicationTestHook::FailAfterIntent
            | PublicationTestHook::FailSettlementOnce
            | PublicationTestHook::FailSettlementPermanently
            | PublicationTestHook::SettlementWriteFault(_)
            | PublicationTestHook::FailAfterSettlementMarkerOnce
            | PublicationTestHook::FailAfterCommittedOutcomeOnce,
        ) => false,
    }
}

#[cfg(test)]
fn take_report_first_move_unsettled(hook: &mut Option<PublicationTestHook>) -> bool {
    if matches!(hook, Some(PublicationTestHook::ReportFirstMoveUnsettled)) {
        *hook = None;
        true
    } else {
        false
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn fail_after_promotions_for_test(version_id: &str, promotions: usize) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::FailAfter { promotions },
        );
}

#[cfg(test)]
pub(crate) fn fail_intent_write_after_promotion_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::IntentWriteFault(ManagedCreateOnlyWriteFault::Promotion),
        );
}

#[cfg(test)]
pub(crate) fn fail_after_intent_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(version_id.to_string(), PublicationTestHook::FailAfterIntent);
}

#[cfg(test)]
pub(crate) fn pause_after_promotions_for_test(
    version_id: &str,
    promotions: usize,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::PauseAfter {
                promotions,
                reached: Some(reached_tx),
                release: Some(release_rx),
            },
        );
    (reached_rx, release_tx)
}

#[cfg(test)]
pub(crate) fn crash_after_artifact_promotion_for_test(
    version_id: &str,
    kind: KnownGoodArtifactKind,
) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::CrashAfterPromotion { kind },
        );
}

#[cfg(test)]
pub(crate) fn report_first_move_unsettled_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::ReportFirstMoveUnsettled,
        );
}

#[cfg(test)]
pub(crate) fn fail_after_committed_outcome_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::FailAfterCommittedOutcomeOnce,
        );
}

#[cfg(test)]
pub(crate) fn fail_settlement_once_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::FailSettlementOnce,
        );
}

#[cfg(test)]
pub(crate) fn fail_settlement_permanently_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::FailSettlementPermanently,
        );
}

#[cfg(test)]
pub(crate) fn fail_after_settlement_marker_for_test(version_id: &str) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::FailAfterSettlementMarkerOnce,
        );
}

async fn settle_owned_context(
    context: TransactionContext,
    expectation: SettlementExpectation,
) -> Result<VersionBundleTransactionSettledOutcome, VersionBundleTransactionSettlementRetry> {
    let holder = Arc::new(Mutex::new(Some(context)));
    let worker_holder = Arc::clone(&holder);
    let attempted = run_publication_blocking(move || {
        let mut context = worker_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("settlement context is present");
        let mut retained_expectation = expectation;
        let settled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            settle_context(&mut context, &mut retained_expectation)
        }))
        .ok()
        .and_then(Result::ok);
        *worker_holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context);
        (settled, retained_expectation)
    })
    .await
    .ok();
    let (settled, retained_expectation) = attempted.unwrap_or((None, expectation));
    let context = holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("settlement worker restored its context");
    match settled {
        Some(outcome) => {
            let TransactionContext { lease, .. } = context;
            Ok(match outcome {
                PersistedTerminalOutcome::Committed => {
                    VersionBundleTransactionSettledOutcome::Committed(lease)
                }
                PersistedTerminalOutcome::RolledBack { effect } => {
                    VersionBundleTransactionSettledOutcome::RolledBack { lease, effect }
                }
            })
        }
        None => Err(VersionBundleTransactionSettlementRetry {
            context: Arc::new(context),
            expectation: retained_expectation,
        }),
    }
}

pub(crate) async fn settled_version_bundle_matches_root(
    lease: &ManagedRootPublicationLease,
    expected: &std::path::Path,
) -> bool {
    if lease.revalidate().is_err() {
        return false;
    }
    let root = lease.root().clone();
    let expected = expected.to_path_buf();
    let matches = run_publication_blocking(move || {
        root.revalidate()?;
        Ok::<_, LoaderError>(ManagedDir::open_root(&expected)?.identity()? == root.identity()?)
    })
    .await;
    matches!(matches, Ok(Ok(true))) && lease.revalidate().is_ok()
}

pub(crate) async fn revalidate_settled_version_bundle(
    lease: &ManagedRootPublicationLease,
    projection: ManagedComponentProjection<'_>,
) -> bool {
    if lease.revalidate().is_err() {
        return false;
    }
    let Ok(fingerprints) = own_fingerprints(&projection) else {
        return false;
    };
    let root = lease.root().clone();
    let exact = run_publication_blocking(move || {
        root.revalidate()?;
        for fingerprint in fingerprints {
            let Some((parent, name)) = open_canonical_parent_loader(&root, &fingerprint)? else {
                return Ok::<_, LoaderError>(false);
            };
            let Some(observed) = parent.inspect_regular_file(&name)? else {
                return Ok(false);
            };
            if observed.size() != fingerprint.size
                || parent.sha1_guarded_file(&name, &observed, MAX_TIER2_ARTIFACT_BYTES)?
                    != fingerprint.digest
            {
                return Ok(false);
            }
        }
        root.revalidate()?;
        Ok(true)
    })
    .await;
    matches!(exact, Ok(Ok(true))) && lease.revalidate().is_ok()
}

fn settle_context(
    context: &mut TransactionContext,
    expectation: &mut SettlementExpectation,
) -> Result<PersistedTerminalOutcome, LoaderError> {
    if let Some((settlement, settlement_guard)) =
        read_settlement(&context.lane).map_err(publication_error_as_loader)?
    {
        validate_settlement(&settlement).map_err(publication_error_as_loader)?;
        if settlement.intent != context.intent
            || matches!(
                *expectation,
                SettlementExpectation::Proven(expected)
                    if settlement.outcome.outcome != expected
            )
        {
            return Err(LoaderError::Verify(
                "version bundle settlement binding changed".to_string(),
            ));
        }
        *expectation = SettlementExpectation::Proven(settlement.outcome.outcome);
        if !context
            .lane
            .file_guard_matches(SETTLEMENT_NAME, &settlement_guard)?
        {
            return Err(LoaderError::Verify(
                "version bundle settlement identity changed".to_string(),
            ));
        }
        cleanup_settled_lane_contents(&context.lease, &context.lane, &settlement)
            .map_err(publication_error_as_loader)?;
        return Ok(settlement.outcome.outcome);
    }
    let expected_outcome = match *expectation {
        SettlementExpectation::Proven(expected_outcome) => {
            validate_proven_outcome(context, expected_outcome)?;
            expected_outcome
        }
        SettlementExpectation::PendingFailure { effect } => prove_pending_outcome(context, effect)?,
    };
    *expectation = SettlementExpectation::Proven(expected_outcome);
    #[cfg(test)]
    if matches!(
        context.test_hook.as_ref(),
        Some(PublicationTestHook::FailSettlementOnce)
    ) {
        context.test_hook = None;
        return Err(LoaderError::Verify(
            "injected version bundle settlement failure".to_string(),
        ));
    }
    #[cfg(test)]
    if matches!(
        context.test_hook.as_ref(),
        Some(PublicationTestHook::FailSettlementPermanently)
    ) {
        return Err(LoaderError::Verify(
            "injected permanent version bundle settlement failure".to_string(),
        ));
    }
    let outcome = PersistedOutcome {
        schema: OUTCOME_SCHEMA.to_string(),
        transaction_nonce: context.intent.transaction_nonce.clone(),
        outcome: expected_outcome,
    };
    let settlement = PersistedSettlement {
        schema: SETTLEMENT_SCHEMA.to_string(),
        phase: PersistedSettlementPhase::CallerSettled,
        generation_nonce: uuid::Uuid::new_v4().simple().to_string(),
        intent: context.intent.clone(),
        outcome,
    };
    #[cfg(test)]
    let settlement_write_fault = match context.test_hook.take() {
        Some(PublicationTestHook::SettlementWriteFault(fault)) => Some(fault),
        retained => {
            context.test_hook = retained;
            None
        }
    };
    let settlement_guard = write_settlement_inner(
        &context.lane,
        &settlement,
        #[cfg(test)]
        settlement_write_fault,
    )
    .map_err(|_| {
        LoaderError::Verify("version bundle settlement remains indeterminate".to_string())
    })?;
    context.lease.revalidate().map_err(publication_as_loader)?;
    if !context
        .lane
        .file_guard_matches(SETTLEMENT_NAME, &settlement_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle settlement identity changed".to_string(),
        ));
    }
    #[cfg(test)]
    if matches!(
        context.test_hook.as_ref(),
        Some(PublicationTestHook::FailAfterSettlementMarkerOnce)
    ) {
        context.test_hook = None;
        return Err(LoaderError::Verify(
            "injected version bundle post-marker settlement failure".to_string(),
        ));
    }
    cleanup_settled_lane_contents(&context.lease, &context.lane, &settlement)
        .map_err(publication_error_as_loader)?;
    Ok(expected_outcome)
}

fn validate_proven_outcome(
    context: &TransactionContext,
    expected_outcome: PersistedTerminalOutcome,
) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(INTENT_NAME, &context.intent_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle proven outcome binding changed".to_string(),
        ));
    }
    let expected_guard = context.outcome_guard.as_ref().ok_or_else(|| {
        LoaderError::Verify("version bundle proven outcome guard is absent".to_string())
    })?;
    let (outcome, observed_guard) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle proven outcome is absent".to_string())
        })?;
    if !context
        .lane
        .file_guard_matches(OUTCOME_NAME, expected_guard)?
        || observed_guard.identity() != expected_guard.identity()
        || !context
            .lane
            .file_guard_matches(OUTCOME_NAME, &observed_guard)?
        || validate_outcome(&outcome, &context.intent).is_err()
        || outcome.outcome != expected_outcome
    {
        return Err(LoaderError::Verify(
            "version bundle proven outcome changed".to_string(),
        ));
    }
    validate_exact_terminal_shape(context, expected_outcome)
}

fn prove_pending_outcome(
    context: &mut TransactionContext,
    effect: VersionBundleTransactionEffect,
) -> Result<PersistedTerminalOutcome, LoaderError> {
    context.lease.root().settle()?;
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(INTENT_NAME, &context.intent_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle pending reconciliation binding changed".to_string(),
        ));
    }
    if let Some((outcome, guard)) =
        read_outcome(&context.lane).map_err(publication_error_as_loader)?
    {
        let retained = context.outcome_guard.as_ref().ok_or_else(|| {
            LoaderError::Verify(
                "version bundle pending outcome has no retained identity".to_string(),
            )
        })?;
        if guard.identity() != retained.identity()
            || !context.lane.file_guard_matches(OUTCOME_NAME, retained)?
        {
            return Err(LoaderError::Verify(
                "version bundle pending outcome identity changed".to_string(),
            ));
        }
        validate_outcome(&outcome, &context.intent).map_err(publication_error_as_loader)?;
        validate_exact_terminal_shape(context, outcome.outcome)?;
        return Ok(outcome.outcome);
    }
    if context.outcome_guard.is_some() {
        return Err(LoaderError::Verify(
            "version bundle retained outcome disappeared".to_string(),
        ));
    }

    let outcome = match reconcile_unfinished_moves(
        &context.lease,
        &context.staging,
        &context.quarantine,
        &context.intent,
    )
    .map_err(publication_error_as_loader)?
    {
        UnfinishedMoveOutcome::Committed => PersistedTerminalOutcome::Committed,
        UnfinishedMoveOutcome::RolledBack => PersistedTerminalOutcome::RolledBack { effect },
    };
    validate_exact_terminal_shape(context, outcome)?;
    let guard = match write_outcome(&context.lane, &context.intent, outcome) {
        Ok(guard) => guard,
        Err(OutcomeWriteFailure::BeforePromotion) => {
            return Err(LoaderError::Verify(
                "version bundle outcome publication failed before promotion".to_string(),
            ));
        }
        Err(OutcomeWriteFailure::PromotionAttempted(Some(guard))) => {
            context.outcome_guard = Some(guard);
            return Err(LoaderError::Verify(
                "version bundle outcome publication remains unsettled".to_string(),
            ));
        }
        Err(OutcomeWriteFailure::PromotionAttempted(None)) => {
            return Err(LoaderError::Verify(
                "version bundle outcome identity remains indeterminate".to_string(),
            ));
        }
    };
    context.outcome_guard = Some(guard);
    let expected_guard = context.outcome_guard.as_ref().ok_or_else(|| {
        LoaderError::Verify("version bundle reconciled outcome guard is absent".to_string())
    })?;
    let (persisted, observed_guard) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle reconciled outcome is absent".to_string())
        })?;
    validate_outcome(&persisted, &context.intent).map_err(publication_error_as_loader)?;
    if observed_guard.identity() != expected_guard.identity()
        || !context
            .lane
            .file_guard_matches(OUTCOME_NAME, expected_guard)?
        || persisted.outcome != outcome
    {
        return Err(LoaderError::Verify(
            "version bundle reconciled outcome changed".to_string(),
        ));
    }
    Ok(persisted.outcome)
}

fn validate_exact_terminal_shape(
    context: &TransactionContext,
    outcome: PersistedTerminalOutcome,
) -> Result<(), LoaderError> {
    validate_slot_topology(&context.staging, &context.quarantine, &context.intent)
        .map_err(publication_error_as_loader)?;
    let fingerprints =
        validate_persisted_intent(&context.intent).map_err(publication_error_as_loader)?;
    for (fingerprint, persisted) in fingerprints.iter().zip(&context.intent.entries) {
        let observed = observe_recovery_entry(
            context.lease.root(),
            &context.staging,
            &context.quarantine,
            fingerprint,
            persisted,
        )
        .map_err(publication_error_as_loader)?;
        let exact = match outcome {
            PersistedTerminalOutcome::Committed => committed_terminal_shape_is_valid(
                &persisted.prior,
                &fingerprint.digest,
                fingerprint.size,
                observed.canonical.state(),
                observed.stage.is_some(),
                observed.quarantine.is_some(),
            ),
            PersistedTerminalOutcome::RolledBack { .. } => {
                observed.stage.is_some()
                    && managed_settled_terminal_shape_is_valid(
                        false,
                        &persisted.prior,
                        &fingerprint.digest,
                        fingerprint.size,
                        observed.canonical.state(),
                        observed.quarantine.is_some(),
                    )
            }
        };
        if !exact {
            return Err(LoaderError::Verify(
                "version bundle reconciled terminal shape is not exact".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_durable_terminal_shape(
    lease: &ManagedRootPublicationLease,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
    outcome: PersistedTerminalOutcome,
) -> Result<(), VersionBundleTransactionError> {
    validate_slot_topology(staging, quarantine, intent)?;
    let fingerprints = validate_persisted_intent(intent)?;
    for (fingerprint, persisted) in fingerprints.iter().zip(&intent.entries) {
        let observed =
            observe_recovery_entry(lease.root(), staging, quarantine, fingerprint, persisted)?;
        if !managed_settled_terminal_shape_is_valid(
            outcome == PersistedTerminalOutcome::Committed,
            &persisted.prior,
            &fingerprint.digest,
            fingerprint.size,
            observed.canonical.state(),
            observed.quarantine.is_some(),
        ) {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)
}

fn validate_settled_lane_shape(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
) -> Result<(), VersionBundleTransactionError> {
    validate_settlement(settlement)?;
    let names = exact_names(
        lane,
        &[
            STAGING_NAME,
            QUARANTINE_NAME,
            INTENT_NAME,
            OUTCOME_NAME,
            SETTLEMENT_NAME,
        ],
        MAX_LANE_ENTRIES,
    )?;
    if !names.contains(STAGING_NAME)
        || !names.contains(QUARANTINE_NAME)
        || !names.contains(SETTLEMENT_NAME)
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    if let Some((intent, _)) = read_intent(lane)?
        && intent != settlement.intent
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    if let Some((outcome, _)) = read_outcome(lane)?
        && outcome != settlement.outcome
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let staging = lane
        .open_child(STAGING_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let quarantine = lane
        .open_child(QUARANTINE_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    validate_durable_terminal_shape(
        lease,
        &staging,
        &quarantine,
        &settlement.intent,
        settlement.outcome.outcome,
    )
}

fn validate_clean_settled_terminal_shape(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
) -> Result<(), VersionBundleTransactionError> {
    let staging = lane
        .open_child(STAGING_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let quarantine = lane
        .open_child(QUARANTINE_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    validate_durable_terminal_shape(
        lease,
        &staging,
        &quarantine,
        &settlement.intent,
        settlement.outcome.outcome,
    )
}

fn cleanup_settled_lane_contents(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
) -> Result<(), VersionBundleTransactionError> {
    validate_settlement(settlement)?;
    let names = exact_names(
        lane,
        &[
            STAGING_NAME,
            QUARANTINE_NAME,
            INTENT_NAME,
            OUTCOME_NAME,
            SETTLEMENT_NAME,
        ],
        MAX_LANE_ENTRIES,
    )?;
    if !names.contains(STAGING_NAME) || !names.contains(QUARANTINE_NAME) {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let staging = lane
        .open_child(STAGING_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    let quarantine = lane
        .open_child(QUARANTINE_NAME)
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    validate_slot_topology(&staging, &quarantine, &settlement.intent)?;
    if let Some((intent, _)) = read_intent(lane)?
        && intent != settlement.intent
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    if let Some((outcome, _)) = read_outcome(lane)?
        && outcome != settlement.outcome
    {
        return Err(VersionBundleTransactionError::RecoveryAmbiguous);
    }
    let fingerprints = validate_persisted_intent(&settlement.intent)?;
    let observations = fingerprints
        .iter()
        .zip(&settlement.intent.entries)
        .map(|(fingerprint, persisted)| {
            observe_recovery_entry(lease.root(), &staging, &quarantine, fingerprint, persisted)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for ((fingerprint, persisted), observed) in fingerprints
        .iter()
        .zip(&settlement.intent.entries)
        .zip(&observations)
    {
        if !managed_settled_terminal_shape_is_valid(
            settlement.outcome.outcome == PersistedTerminalOutcome::Committed,
            &persisted.prior,
            &fingerprint.digest,
            fingerprint.size,
            observed.canonical.state(),
            observed.quarantine.is_some(),
        ) {
            return Err(VersionBundleTransactionError::RecoveryAmbiguous);
        }
    }
    for (persisted, observed) in settlement.intent.entries.iter().zip(observations) {
        if let Some(stage) = observed.stage {
            staging
                .remove_guarded_file(&persisted.staging_slot, &stage)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        }
        if let Some(quarantined) = observed.quarantine {
            quarantine
                .remove_guarded_file(&persisted.quarantine_slot, &quarantined)
                .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
        }
    }
    if let Some((_, outcome_guard)) = read_outcome(lane)? {
        lane.remove_guarded_file(OUTCOME_NAME, &outcome_guard)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    }
    if let Some((_, intent_guard)) = read_intent(lane)? {
        lane.remove_guarded_file(INTENT_NAME, &intent_guard)
            .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    }
    lease
        .revalidate()
        .map_err(|_| VersionBundleTransactionError::RecoveryAmbiguous)?;
    Ok(())
}

fn publication_as_loader(
    error: crate::managed_publication::ManagedPublicationError,
) -> LoaderError {
    LoaderError::Verify(error.to_string())
}

fn publication_error_as_loader(error: VersionBundleTransactionError) -> LoaderError {
    LoaderError::Verify(error.to_string())
}

fn terminal_failure(
    context: TransactionContext,
    effect: VersionBundleTransactionEffect,
) -> VersionBundleTransactionFailureReceipt {
    VersionBundleTransactionFailureReceipt {
        context: Arc::new(context),
        expectation: SettlementExpectation::Proven(PersistedTerminalOutcome::RolledBack { effect }),
    }
}

fn reconciliation_failure(
    context: TransactionContext,
    effect: VersionBundleTransactionEffect,
) -> VersionBundleTransactionFailureReceipt {
    VersionBundleTransactionFailureReceipt {
        context: Arc::new(context),
        expectation: SettlementExpectation::PendingFailure { effect },
    }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;
    use sha1::Sha1;

    fn test_sha1(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }

    struct SettlementFixture {
        _temporary: tempfile::TempDir,
        context: TransactionContext,
        lane: ManagedDir,
        staging: ManagedDir,
        quarantine: ManagedDir,
        version_parent: ManagedDir,
        version_id: &'static str,
    }

    async fn pending_settlement_fixture(test_hook: PublicationTestHook) -> SettlementFixture {
        let temporary = tempfile::TempDir::new().expect("settlement retry root");
        let root_path = temporary.path().join("library");
        std::fs::create_dir(&root_path).expect("create settlement retry root");
        let root = ManagedDir::open_root(&root_path).expect("open settlement retry root");
        let lease = ManagedRootPublicationLease::acquire(root)
            .await
            .expect("acquire settlement retry lease");
        let root_identity = lease.root().identity().expect("settlement root identity");
        let version_id = "settlement-retry";
        let metadata_source = b"authenticated-version-metadata";
        let client_source = b"authenticated-client-jar";
        let client_prior = b"prior-client-jar";
        let versions = lease
            .root()
            .open_or_create_child("versions")
            .expect("create versions root");
        let version_parent = versions
            .open_or_create_child(version_id)
            .expect("create version parent");
        version_parent
            .write_new_exact(&format!("{version_id}.json"), metadata_source)
            .expect("write promoted metadata fixture");

        let intent = PersistedIntent {
            schema: INTENT_SCHEMA.to_string(),
            phase: PersistedIntentPhase::Prepared,
            version_id: version_id.to_string(),
            activation_contract_id: ManagedInstallActivationContractId::from_digest([7; 32]),
            transaction_nonce: "0123456789abcdef0123456789abcdef".to_string(),
            created_ancestors: Vec::new(),
            entries: vec![
                PersistedEntry {
                    ordinal: 0,
                    root: PhysicalRoot::Versions,
                    relative_path: format!("{version_id}/{version_id}.json"),
                    kind: PersistedArtifactKind::VersionMetadata,
                    source_sha1: test_sha1(metadata_source),
                    source_size: metadata_source.len() as u64,
                    staging_slot: "entry-0".to_string(),
                    quarantine_slot: "entry-0".to_string(),
                    prior: PriorFingerprint::Absent,
                },
                PersistedEntry {
                    ordinal: 1,
                    root: PhysicalRoot::Versions,
                    relative_path: format!("{version_id}/{version_id}.jar"),
                    kind: PersistedArtifactKind::ClientJar,
                    source_sha1: test_sha1(client_source),
                    source_size: client_source.len() as u64,
                    staging_slot: "entry-1".to_string(),
                    quarantine_slot: "entry-1".to_string(),
                    prior: PriorFingerprint::ExistingFile {
                        sha1: test_sha1(client_prior),
                        size: client_prior.len() as u64,
                    },
                },
            ],
        };
        let fingerprints = validate_persisted_intent(&intent).expect("valid settlement intent");
        let lane = open_lane(&lease).expect("open settlement retry lane");
        lane.write_new_exact(
            INTENT_NAME,
            &bounded_marker_bytes(&intent, MAX_MARKER_BYTES).expect("serialize settlement intent"),
        )
        .expect("write settlement intent");
        lane.sync().expect("sync settlement intent");
        let intent_guard = lane
            .inspect_regular_file(INTENT_NAME)
            .expect("inspect settlement intent")
            .expect("settlement intent exists");
        let (staging, quarantine) =
            open_or_create_slots_after_intent(&lease, &lane).expect("create settlement slots");
        staging
            .write_new_exact("entry-1", client_source)
            .expect("write retained client stage");
        quarantine
            .write_new_exact("entry-1", client_prior)
            .expect("write quarantined client prior");
        staging.sync().expect("sync retained client stage");
        quarantine.sync().expect("sync quarantined client prior");

        let entries = fingerprints
            .into_iter()
            .zip(&intent.entries)
            .enumerate()
            .map(|(index, (fingerprint, persisted))| TransactionEntry {
                fingerprint,
                stage_name: persisted.staging_slot.clone(),
                quarantine_name: persisted.quarantine_slot.clone(),
                stage_guard: None,
                canonical_guard: None,
                target: None,
                state: if index == 0 {
                    EntryState::PublishedNew
                } else {
                    EntryState::Quarantined
                },
            })
            .collect();
        let context = TransactionContext {
            lease,
            root_identity,
            lane: lane.clone(),
            staging: staging.clone(),
            quarantine: quarantine.clone(),
            intent,
            intent_guard,
            outcome_guard: None,
            entries,
            test_hook: Some(test_hook),
        };

        SettlementFixture {
            _temporary: temporary,
            context,
            lane,
            staging,
            quarantine,
            version_parent,
            version_id,
        }
    }

    #[tokio::test]
    async fn superseded_intent_and_settlement_schemas_are_rejected_explicitly() {
        let fixture = pending_settlement_fixture(PublicationTestHook::FailAfter {
            promotions: usize::MAX,
        })
        .await;
        let mut old_intent = fixture.context.intent.clone();
        old_intent.schema = "axial.version_bundle_publication.intent.v2".to_string();
        assert!(matches!(
            validate_persisted_intent(&old_intent),
            Err(VersionBundleTransactionError::RecoveryAmbiguous)
        ));

        let mut old_settlement = PersistedSettlement {
            schema: "axial.version_bundle_publication.settlement.v3".to_string(),
            phase: PersistedSettlementPhase::CallerSettled,
            generation_nonce: "abcdef0123456789abcdef0123456789".to_string(),
            outcome: PersistedOutcome {
                schema: OUTCOME_SCHEMA.to_string(),
                transaction_nonce: fixture.context.intent.transaction_nonce.clone(),
                outcome: PersistedTerminalOutcome::Committed,
            },
            intent: fixture.context.intent.clone(),
        };
        assert!(matches!(
            validate_settlement(&old_settlement),
            Err(VersionBundleTransactionError::RecoveryAmbiguous)
        ));
        old_settlement.schema = SETTLEMENT_SCHEMA.to_string();
        assert!(validate_settlement(&old_settlement).is_ok());
    }

    #[tokio::test]
    async fn publication_markers_persist_digests_without_provider_or_source_material() {
        let fixture = pending_settlement_fixture(PublicationTestHook::FailAfter {
            promotions: usize::MAX,
        })
        .await;
        let provider_url = "https://credentials.invalid/provider/source?token=never-persist";
        let mut intent = fixture.context.intent.clone();
        intent.activation_contract_id = ManagedInstallActivationContractId::from_digest(
            Sha256::digest(provider_url.as_bytes()).into(),
        );
        let encoded = String::from_utf8(
            bounded_marker_bytes(&intent, MAX_MARKER_BYTES).expect("encode publication intent"),
        )
        .expect("intent marker is UTF-8 JSON");

        for forbidden in [
            provider_url,
            "authenticated-version-metadata",
            "authenticated-client-jar",
            "provider_url",
            "source_url",
            "source_bytes",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "publication marker leaked source material: {forbidden}"
            );
        }
        assert!(encoded.contains(intent.activation_contract_id.as_str()));
    }

    fn assert_settlement_retained(
        lane: &ManagedDir,
        staging: &ManagedDir,
        quarantine: &ManagedDir,
    ) {
        assert!(read_intent(lane).expect("read cleaned intent").is_none());
        assert!(read_outcome(lane).expect("read cleaned outcome").is_none());
        assert!(
            read_settlement(lane)
                .expect("read retained settlement")
                .is_some()
        );
        assert!(
            staging
                .entries_bounded(1)
                .expect("read cleaned staging")
                .is_empty()
        );
        assert!(
            quarantine
                .entries_bounded(1)
                .expect("read cleaned quarantine")
                .is_empty()
        );
    }

    fn assert_settlement_acknowledged(
        lane: &ManagedDir,
        staging: &ManagedDir,
        quarantine: &ManagedDir,
    ) {
        assert!(
            read_intent(lane)
                .expect("read acknowledged intent")
                .is_none()
        );
        assert!(
            read_outcome(lane)
                .expect("read acknowledged outcome")
                .is_none()
        );
        assert!(
            read_settlement(lane)
                .expect("read acknowledged settlement")
                .is_none()
        );
        assert!(
            staging
                .entries_bounded(1)
                .expect("read acknowledged staging")
                .is_empty()
        );
        assert!(
            quarantine
                .entries_bounded(1)
                .expect("read acknowledged quarantine")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pending_reconciliation_settles_after_one_marker_write_failure() {
        let SettlementFixture {
            _temporary,
            context,
            lane,
            staging,
            quarantine,
            version_parent,
            version_id,
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let settlement = settle_owned_context(
            context,
            SettlementExpectation::PendingFailure {
                effect: VersionBundleTransactionEffect::Rollback,
            },
        )
        .await
        .expect_err("first settlement marker write fails");
        assert!(
            read_outcome(&lane)
                .expect("read reconciled outcome")
                .is_some()
        );
        assert!(
            read_settlement(&lane)
                .expect("read absent settlement")
                .is_none()
        );
        assert!(
            version_parent
                .inspect_regular_file(&format!("{version_id}.json"))
                .expect("inspect rolled-back metadata")
                .is_none()
        );
        assert!(
            staging
                .inspect_regular_file("entry-0")
                .expect("inspect reconciled metadata stage")
                .is_some()
        );
        assert!(
            staging
                .inspect_regular_file("entry-1")
                .expect("inspect retained client stage")
                .is_some()
        );
        assert!(
            quarantine
                .entries_bounded(1)
                .expect("inspect reconciled quarantine")
                .is_empty()
        );

        let lease = match settlement.retry().await.expect("retry settlement") {
            VersionBundleTransactionSettledOutcome::RolledBack {
                lease,
                effect: VersionBundleTransactionEffect::Rollback,
            } => lease,
            _ => panic!("unexpected settlement outcome"),
        };
        assert_settlement_retained(&lane, &staging, &quarantine);
        let (lease, evidence) = match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::one(version_id)
                .expect("exact settlement candidate"),
        )
        .await
        {
            DurableVersionBundleOutcome::RolledBack {
                lease,
                evidence,
                effect: VersionBundleTransactionEffect::Rollback,
            } => (lease, evidence),
            _ => panic!("retained rollback was not classified exactly"),
        };
        assert!(matches!(
            acknowledge_durable_version_bundle(lease, evidence).await,
            DurableVersionBundleAcknowledgementOutcome::Acknowledged(_)
        ));
        assert_settlement_acknowledged(&lane, &staging, &quarantine);
    }

    #[tokio::test]
    async fn two_candidate_classifier_reports_empty_lane_without_provider_work() {
        let temporary = tempfile::tempdir().expect("empty candidate root");
        let root = ManagedDir::open_root(temporary.path()).expect("open empty candidate root");
        let lease = ManagedRootPublicationLease::acquire(root)
            .await
            .expect("acquire empty candidate root");

        assert!(matches!(
            classify_durable_version_bundle_candidates(
                lease,
                ManagedInstallPublicationCandidates::pair("candidate-base", "candidate-child")
                    .expect("exact empty-lane candidates"),
            )
            .await,
            DurableVersionBundleOutcome::NoEffect(_)
        ));
    }

    #[tokio::test]
    async fn two_candidate_classifier_accepts_base_or_rolled_back_child_and_rejects_third_id() {
        let SettlementFixture {
            _temporary,
            context,
            lane,
            staging,
            quarantine,
            version_id,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let retry = settle_owned_context(
            context,
            SettlementExpectation::PendingFailure {
                effect: VersionBundleTransactionEffect::Rollback,
            },
        )
        .await
        .expect_err("first settlement marker write fails");
        let lease = match retry.retry().await.expect("settle rollback marker") {
            VersionBundleTransactionSettledOutcome::RolledBack { lease, .. } => lease,
            _ => panic!("unexpected settlement outcome"),
        };
        assert_settlement_retained(&lane, &staging, &quarantine);

        let lease = match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::pair("unexpected-a", "unexpected-b")
                .expect("exact unexpected candidates"),
        )
        .await
        {
            DurableVersionBundleOutcome::Indeterminate(lease) => lease,
            _ => panic!("unexpected third settlement identity did not fail closed"),
        };
        let (lease, evidence) = match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::pair(version_id, "candidate-child")
                .expect("exact base candidates"),
        )
        .await
        {
            DurableVersionBundleOutcome::RolledBack {
                lease,
                evidence,
                effect: VersionBundleTransactionEffect::Rollback,
            } => (lease, evidence),
            _ => panic!("base candidate did not classify retained rollback"),
        };
        assert_eq!(evidence.version_id(), version_id);
        assert!(matches!(
            acknowledge_durable_version_bundle(lease, evidence).await,
            DurableVersionBundleAcknowledgementOutcome::Acknowledged(_)
        ));

        let SettlementFixture {
            _temporary,
            context,
            version_id,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let retry = settle_owned_context(
            context,
            SettlementExpectation::PendingFailure {
                effect: VersionBundleTransactionEffect::Rollback,
            },
        )
        .await
        .expect_err("first child settlement marker write fails");
        let lease = match retry.retry().await.expect("settle child rollback marker") {
            VersionBundleTransactionSettledOutcome::RolledBack { lease, .. } => lease,
            _ => panic!("unexpected child settlement outcome"),
        };
        match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::pair("candidate-base", version_id)
                .expect("exact child candidates"),
        )
        .await
        {
            DurableVersionBundleOutcome::RolledBack { evidence, .. } => {
                assert_eq!(evidence.version_id(), version_id);
            }
            _ => panic!("child candidate did not classify retained rollback"),
        }
    }

    #[tokio::test]
    async fn two_candidate_classifier_reports_committed_child_without_provider_work() {
        let temporary = tempfile::tempdir().expect("committed candidate root");
        let root = ManagedDir::open_root(temporary.path()).expect("open committed candidate root");
        let reconstruction =
            crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
                root,
                "candidate-child",
            )
            .expect("build committed candidate fixture");
        let (root, projection, source) = reconstruction.into_effect_parts();
        let lease = ManagedRootPublicationLease::acquire(root)
            .await
            .expect("acquire committed candidate root");
        let activation_contract_id = projection
            .activation_contract_id()
            .expect("derive committed candidate contract");
        let projection = projection
            .component_projection()
            .expect("project committed candidate fixture");
        let publication =
            publish_version_bundle(lease, source, activation_contract_id.clone(), projection).await;
        let lease = match settle_version_bundle_publication(publication)
            .await
            .expect("settle committed candidate")
        {
            VersionBundleTransactionSettledOutcome::Committed(lease) => lease,
            _ => panic!("candidate child publication rolled back"),
        };

        let (lease, evidence) = match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::pair("candidate-base", "candidate-child")
                .expect("exact committed candidates"),
        )
        .await
        {
            DurableVersionBundleOutcome::Committed { lease, evidence } => (lease, evidence),
            _ => panic!("committed child candidate did not classify"),
        };
        assert_eq!(evidence.version_id(), "candidate-child");
        assert_eq!(
            evidence.committed_activation_contract_id(),
            Some(&activation_contract_id),
            "restart classification must expose the contract persisted before publication"
        );
        assert!(matches!(
            acknowledge_durable_version_bundle(lease, evidence).await,
            DurableVersionBundleAcknowledgementOutcome::Acknowledged(_)
        ));
    }

    #[tokio::test]
    async fn settlement_promotion_before_sync_retains_one_restart_generation() {
        let SettlementFixture {
            _temporary: temporary,
            context,
            lane,
            staging,
            quarantine,
            version_id,
            ..
        } = pending_settlement_fixture(PublicationTestHook::SettlementWriteFault(
            ManagedCreateOnlyWriteFault::Promotion,
        ))
        .await;
        let lease = match settle_owned_context(
            context,
            SettlementExpectation::PendingFailure {
                effect: VersionBundleTransactionEffect::Rollback,
            },
        )
        .await
        .expect("promotion recovery must synchronize the settlement namespace")
        {
            VersionBundleTransactionSettledOutcome::RolledBack {
                lease,
                effect: VersionBundleTransactionEffect::Rollback,
            } => lease,
            _ => panic!("unexpected promotion recovery outcome"),
        };
        assert_settlement_retained(&lane, &staging, &quarantine);

        let (lease, first_evidence) = match classify_durable_version_bundle_candidates(
            lease,
            ManagedInstallPublicationCandidates::one(version_id)
                .expect("exact settlement candidate"),
        )
        .await
        {
            DurableVersionBundleOutcome::RolledBack {
                lease,
                evidence,
                effect: VersionBundleTransactionEffect::Rollback,
            } => (lease, evidence),
            _ => panic!("synchronized settlement did not classify"),
        };
        let first_generation = first_evidence.settlement_generation().to_string();
        let first_binding = first_evidence.root_binding().to_string();
        let first_fingerprint = first_evidence.fingerprint().to_string();
        drop((lease, first_evidence));

        let reopened = ManagedDir::open_root(&temporary.path().join("library"))
            .expect("reopen settlement root");
        let restarted_lease = ManagedRootPublicationLease::acquire(reopened)
            .await
            .expect("reacquire settlement root after restart");
        let (restarted_lease, restarted_evidence) =
            match classify_durable_version_bundle_candidates(
                restarted_lease,
                ManagedInstallPublicationCandidates::one(version_id)
                    .expect("exact restart candidate"),
            )
            .await
            {
                DurableVersionBundleOutcome::RolledBack {
                    lease,
                    evidence,
                    effect: VersionBundleTransactionEffect::Rollback,
                } => (lease, evidence),
                _ => panic!("restart did not recover the synchronized settlement"),
            };
        assert_eq!(restarted_evidence.settlement_generation(), first_generation);
        assert_eq!(restarted_evidence.root_binding(), first_binding);
        assert_eq!(restarted_evidence.fingerprint(), first_fingerprint);
        assert!(matches!(
            acknowledge_durable_version_bundle(restarted_lease, restarted_evidence).await,
            DurableVersionBundleAcknowledgementOutcome::Acknowledged(_)
        ));
        assert_settlement_acknowledged(&lane, &staging, &quarantine);
    }

    #[tokio::test]
    async fn settlement_recovery_retains_exact_context_and_root_lease() {
        let SettlementFixture {
            _temporary,
            context,
            lane,
            staging,
            quarantine,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let context = Arc::new(context);
        let retained_context = Arc::clone(&context);
        let publication = Err(VersionBundleTransactionError::Effect(Box::new(
            VersionBundleTransactionFailureReceipt {
                context,
                expectation: SettlementExpectation::PendingFailure {
                    effect: VersionBundleTransactionEffect::Rollback,
                },
            },
        )));
        let recovery = match settle_version_bundle_publication(publication).await {
            Err(VersionBundleTransactionError::Indeterminate(recovery)) => recovery,
            other => panic!("shared settlement context did not remain recoverable: {other:?}"),
        };
        let competing_root =
            ManagedDir::open_root(&_temporary.path().join("library")).expect("open competing root");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                ManagedRootPublicationLease::acquire(competing_root),
            )
            .await
            .is_err(),
            "recovery must retain the exclusive root lease"
        );

        drop(retained_context);
        assert!(matches!(
            recovery.retry().await.expect("resume retained settlement"),
            VersionBundleTransactionSettledOutcome::RolledBack {
                effect: VersionBundleTransactionEffect::Rollback,
                ..
            }
        ));
        assert_settlement_retained(&lane, &staging, &quarantine);
    }

    #[tokio::test]
    async fn pending_reconciliation_rejects_replaced_outcome_identity() {
        let SettlementFixture {
            _temporary,
            mut context,
            lane,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let effect = VersionBundleTransactionEffect::Promotion;
        prove_pending_outcome(&mut context, effect).expect("publish exact pending outcome");
        let retained_identity = context
            .outcome_guard
            .as_ref()
            .expect("retain pending outcome")
            .identity();
        let (outcome, guard) = read_outcome(&lane)
            .expect("read pending outcome")
            .expect("pending outcome exists");
        let bytes =
            bounded_marker_bytes(&outcome, MAX_MARKER_BYTES).expect("encode replacement outcome");
        lane.remove_guarded_file(OUTCOME_NAME, &guard)
            .expect("remove exact pending outcome");
        lane.write_new_exact(OUTCOME_NAME, &bytes)
            .expect("write same-content replacement outcome");

        assert!(
            prove_pending_outcome(&mut context, effect).is_err(),
            "same-content marker replacement must remain indeterminate"
        );
        assert_eq!(
            context
                .outcome_guard
                .as_ref()
                .expect("retained outcome survives rejection")
                .identity(),
            retained_identity
        );
    }

    #[tokio::test]
    async fn pending_reconciliation_rejects_disappeared_outcome_identity() {
        let SettlementFixture {
            _temporary,
            mut context,
            lane,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailSettlementOnce).await;
        let effect = VersionBundleTransactionEffect::Promotion;
        prove_pending_outcome(&mut context, effect).expect("publish exact pending outcome");
        let retained_identity = context
            .outcome_guard
            .as_ref()
            .expect("retain pending outcome")
            .identity();
        let (_, guard) = read_outcome(&lane)
            .expect("read pending outcome")
            .expect("pending outcome exists");
        lane.remove_guarded_file(OUTCOME_NAME, &guard)
            .expect("remove exact pending outcome");

        assert!(
            prove_pending_outcome(&mut context, effect).is_err(),
            "disappeared retained marker must remain indeterminate"
        );
        assert!(
            read_outcome(&lane)
                .expect("read absent pending outcome")
                .is_none(),
            "recovery must not publish a replacement marker"
        );
        assert_eq!(
            context
                .outcome_guard
                .as_ref()
                .expect("retained outcome survives rejection")
                .identity(),
            retained_identity
        );
    }

    #[tokio::test]
    async fn malformed_active_intent_returns_indeterminate_without_livelock() {
        let temporary = tempfile::tempdir().expect("malformed intent root");
        let root =
            ManagedDir::open_root(temporary.path()).expect("open malformed intent managed root");
        let reconstruction =
            crate::known_good::managed_version_bundle_reconstruction_fixture_for_test(
                root,
                "malformed-active-intent",
            )
            .expect("build malformed intent fixture");
        let (root, projection, source) = reconstruction.into_effect_parts();
        let lease = ManagedRootPublicationLease::acquire(root)
            .await
            .expect("acquire malformed intent lease");
        let lane = open_lane(&lease).expect("open malformed intent lane");
        lane.write_new_exact(INTENT_NAME, b"{")
            .expect("publish malformed active intent");
        let component = projection
            .component_projection()
            .expect("project malformed intent fixture");
        let activation_contract_id = projection
            .activation_contract_id()
            .expect("derive malformed intent contract");

        let started = std::time::Instant::now();
        let error =
            match publish_version_bundle(lease, source, activation_contract_id, component).await {
                Ok(receipt) => {
                    drop(receipt);
                    panic!("malformed active intent unexpectedly committed")
                }
                Err(error) => error,
            };
        let recovery = match error {
            VersionBundleTransactionError::Indeterminate(recovery) => recovery,
            other => panic!("malformed intent did not retain recovery: {other:?}"),
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "permanent structural ambiguity must not hold the publication owner forever"
        );
        let competing_root =
            ManagedDir::open_root(temporary.path()).expect("open competing malformed root");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                ManagedRootPublicationLease::acquire(competing_root),
            )
            .await
            .is_err(),
            "preparation recovery must retain the exclusive root lease"
        );
        let intent_guard = lane
            .inspect_regular_file(INTENT_NAME)
            .expect("inspect malformed intent")
            .expect("malformed intent remains visible");
        lane.remove_guarded_file(INTENT_NAME, &intent_guard)
            .expect("remove malformed intent under retained root authority");
        lane.sync().expect("settle malformed intent removal");
        assert!(matches!(
            recovery.retry().await.expect("resume repaired preparation"),
            VersionBundleTransactionSettledOutcome::Committed(_)
        ));
    }

    #[tokio::test]
    async fn marker_backed_cleanup_resumes_after_one_post_marker_failure() {
        let SettlementFixture {
            _temporary,
            context,
            lane,
            staging,
            quarantine,
            ..
        } = pending_settlement_fixture(PublicationTestHook::FailAfterSettlementMarkerOnce).await;

        let settlement = settle_owned_context(
            context,
            SettlementExpectation::PendingFailure {
                effect: VersionBundleTransactionEffect::Rollback,
            },
        )
        .await
        .expect_err("post-marker settlement cleanup fails once");
        assert!(
            read_settlement(&lane)
                .expect("read retained settlement")
                .is_some()
        );
        assert!(
            read_outcome(&lane)
                .expect("read retained outcome")
                .is_some()
        );
        assert_eq!(
            staging
                .entries_bounded(MAX_VERSION_BUNDLE_ENTRIES)
                .expect("read retained stages")
                .len(),
            2
        );
        assert!(matches!(
            settlement.retry().await.expect("resume marker cleanup"),
            VersionBundleTransactionSettledOutcome::RolledBack {
                effect: VersionBundleTransactionEffect::Rollback,
                ..
            }
        ));
        assert_settlement_retained(&lane, &staging, &quarantine);
    }
}
