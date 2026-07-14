use crate::download::{AuthenticatedVersionBundleMemberSource, AuthenticatedVersionBundleSource};
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRelativePath, KnownGoodRoot,
    MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection,
    ManagedKnownGoodComponent,
};
use crate::loaders::LoaderError;
use crate::managed_fs::{ManagedDir, ManagedDirectoryIdentity, ManagedFileGuard};
use crate::managed_publication::{ManagedRootPublicationLease, run_publication_blocking};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
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
const INTENT_SCHEMA: &str = "axial.version_bundle_publication.intent.v1";
const OUTCOME_SCHEMA: &str = "axial.version_bundle_publication.outcome.v1";
const SETTLEMENT_SCHEMA: &str = "axial.version_bundle_publication.settlement.v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum PriorFingerprint {
    Absent,
    ExistingFile { sha1: String, size: u64 },
}

impl PriorFingerprint {
    fn matches_source(&self, source: &EntryFingerprint) -> bool {
        matches!(
            self,
            Self::ExistingFile { sha1, size }
                if *size == source.size && sha1 == &source.digest
        )
    }
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
    #[cfg(test)]
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

#[cfg(test)]
enum PublicationTestHook {
    FailAfter {
        promotions: usize,
    },
    PauseAfter {
        promotions: usize,
        reached: Option<tokio::sync::oneshot::Sender<()>>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    },
    CrashAfterPromotion {
        kind: KnownGoodArtifactKind,
    },
    CrashAfterQuarantine {
        kind: KnownGoodArtifactKind,
    },
    FailSettlementOnce,
    FailAfterSettlementMarkerOnce,
    FailAfterCommittedOutcomeOnce,
}

#[cfg(test)]
static TEST_HOOKS: OnceLock<Mutex<HashMap<String, PublicationTestHook>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleDisposition {
    AlreadyExact,
    PublishedNew,
    ReplacedWithQuarantine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedVersionBundleOrdinalDisposition {
    ordinal: usize,
    disposition: ManagedVersionBundleDisposition,
}

impl ManagedVersionBundleOrdinalDisposition {
    pub fn inventory_ordinal(self) -> usize {
        self.ordinal
    }

    pub fn disposition(self) -> ManagedVersionBundleDisposition {
        self.disposition
    }
}

pub struct ManagedVersionBundleCommitReceipt {
    context: Arc<TransactionContext>,
    dispositions: Vec<ManagedVersionBundleOrdinalDisposition>,
}

pub struct ManagedVersionBundleFailureReceipt {
    context: Arc<TransactionContext>,
    effect: ManagedVersionBundleEffect,
    expectation: SettlementExpectation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettlementExpectation {
    Proven(PersistedTerminalOutcome),
    PendingFailure { effect: ManagedVersionBundleEffect },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleSettlementOutcome {
    Committed,
    RolledBack { effect: ManagedVersionBundleEffect },
}

impl From<PersistedTerminalOutcome> for ManagedVersionBundleSettlementOutcome {
    fn from(outcome: PersistedTerminalOutcome) -> Self {
        match outcome {
            PersistedTerminalOutcome::Committed => Self::Committed,
            PersistedTerminalOutcome::RolledBack { effect } => Self::RolledBack { effect },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedVersionBundleEffect {
    Promotion,
    Postcheck,
    Rollback,
}

impl std::fmt::Debug for ManagedVersionBundleCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedVersionBundleCommitReceipt")
            .field("entry_count", &self.context.entries.len())
            .field("dispositions", &self.dispositions)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedVersionBundleFailureReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedVersionBundleFailureReceipt")
            .field("entry_count", &self.context.entries.len())
            .field("effect", &self.effect)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedVersionBundlePublicationError {
    #[error("version bundle source does not match the admitted projection")]
    ProjectionMismatch,
    #[error("version bundle projection contains a portable path alias")]
    PortablePathAlias,
    #[error("version bundle publication lane belongs to another exact projection")]
    LaneOccupied,
    #[error("version bundle publication recovery is ambiguous")]
    RecoveryAmbiguous,
    #[error("version bundle publication preparation failed")]
    Preparation,
    #[error("version bundle publication task stopped unexpectedly")]
    TaskStopped,
    #[error("version bundle publication effects failed")]
    Effect(Box<ManagedVersionBundleFailureReceipt>),
}

pub struct ManagedVersionBundleSettlementFailure {
    context: Arc<TransactionContext>,
    expectation: SettlementExpectation,
}

impl std::fmt::Debug for ManagedVersionBundleSettlementFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedVersionBundleSettlementFailure")
            .field("expectation", &self.expectation)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for ManagedVersionBundleSettlementFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("version bundle receipt settlement remains retryable")
    }
}

impl std::error::Error for ManagedVersionBundleSettlementFailure {}

impl ManagedVersionBundleSettlementFailure {
    pub async fn retry(
        self,
    ) -> Result<ManagedVersionBundleSettlementOutcome, ManagedVersionBundleSettlementFailure> {
        let context = Arc::try_unwrap(self.context).map_err(|context| Self {
            context,
            expectation: self.expectation,
        })?;
        settle_owned_context(context, self.expectation).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedVersionBundleRebuildError {
    #[error("version bundle source is unavailable")]
    SourceUnavailable,
    #[error("managed version bundle root is unavailable")]
    RootUnavailable,
    #[error(transparent)]
    Publication(#[from] ManagedVersionBundlePublicationError),
}

impl ManagedVersionBundleRebuildError {
    pub fn failure_phase(&self) -> ManagedVersionBundleFailurePhase {
        match self {
            Self::SourceUnavailable | Self::RootUnavailable => {
                ManagedVersionBundleFailurePhase::PreEffect
            }
            Self::Publication(error) => error.failure_phase(),
        }
    }

    pub fn into_effect_receipt(self) -> Option<ManagedVersionBundleFailureReceipt> {
        match self {
            Self::Publication(error) => error.into_effect_receipt(),
            Self::SourceUnavailable | Self::RootUnavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedVersionBundleFailurePhase {
    PreEffect,
    Effect,
}

impl ManagedVersionBundlePublicationError {
    pub fn failure_phase(&self) -> ManagedVersionBundleFailurePhase {
        match self {
            Self::ProjectionMismatch
            | Self::PortablePathAlias
            | Self::LaneOccupied
            | Self::RecoveryAmbiguous
            | Self::Preparation => ManagedVersionBundleFailurePhase::PreEffect,
            Self::TaskStopped | Self::Effect(_) => ManagedVersionBundleFailurePhase::Effect,
        }
    }

    pub fn into_effect_receipt(self) -> Option<ManagedVersionBundleFailureReceipt> {
        match self {
            Self::Effect(receipt) => Some(*receipt),
            Self::ProjectionMismatch
            | Self::PortablePathAlias
            | Self::LaneOccupied
            | Self::RecoveryAmbiguous
            | Self::Preparation
            | Self::TaskStopped => None,
        }
    }
}

impl ManagedVersionBundleCommitReceipt {
    pub fn dispositions(&self) -> &[ManagedVersionBundleOrdinalDisposition] {
        &self.dispositions
    }

    pub fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        projection_matches_fingerprints(projection, fingerprints(&self.context))
    }

    pub(crate) fn matches_root_identity(&self, identity: ManagedDirectoryIdentity) -> bool {
        identity == self.context.root_identity
    }

    pub async fn revalidate(&self) -> bool {
        let context = Arc::clone(&self.context);
        run_publication_blocking(move || revalidate_committed(&context).is_ok())
            .await
            .is_ok_and(|valid| valid)
    }

    pub async fn settle(
        self,
    ) -> Result<ManagedVersionBundleSettlementOutcome, ManagedVersionBundleSettlementFailure> {
        let context = Arc::try_unwrap(self.context).map_err(|context| {
            ManagedVersionBundleSettlementFailure {
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

impl ManagedVersionBundleFailureReceipt {
    pub fn effect(&self) -> ManagedVersionBundleEffect {
        self.effect
    }

    pub fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        projection_matches_fingerprints(projection, fingerprints(&self.context))
    }

    pub(crate) fn matches_root_identity(&self, identity: ManagedDirectoryIdentity) -> bool {
        identity == self.context.root_identity
    }

    pub async fn revalidate(&self) -> bool {
        if matches!(
            self.expectation,
            SettlementExpectation::PendingFailure { .. }
        ) {
            return false;
        }
        let context = Arc::clone(&self.context);
        run_publication_blocking(move || revalidate_failure(&context).is_ok())
            .await
            .is_ok_and(|valid| valid)
    }

    pub async fn settle(
        self,
    ) -> Result<ManagedVersionBundleSettlementOutcome, ManagedVersionBundleSettlementFailure> {
        let expectation = self.expectation;
        let context = Arc::try_unwrap(self.context).map_err(|context| {
            ManagedVersionBundleSettlementFailure {
                context,
                expectation,
            }
        })?;
        settle_owned_context(context, expectation).await
    }
}

enum PreparationOutcome {
    Ready(Box<TransactionContext>),
    Committed(ManagedVersionBundleCommitReceipt),
    RolledBack(ManagedVersionBundleFailureReceipt),
}

pub(crate) async fn publish_version_bundle(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    projection: ManagedComponentProjection<'_>,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundlePublicationError> {
    if !source.matches_projection(&projection) {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    let version_id = source.version_id().to_string();
    #[cfg(test)]
    let test_hook = TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&version_id);
    let fingerprints = own_fingerprints(&projection)?;
    validate_portable_aliases(&fingerprints)?;
    validate_bundle_topology(&version_id, &fingerprints)?;
    #[cfg(test)]
    let preparation =
        move || prepare_transaction(lease, source, version_id, fingerprints, test_hook);
    #[cfg(not(test))]
    let preparation = move || prepare_transaction(lease, source, version_id, fingerprints);
    let preparation = run_publication_blocking(preparation)
        .await
        .map_err(|_| ManagedVersionBundlePublicationError::TaskStopped)??;
    let context = match preparation {
        PreparationOutcome::Ready(context) => *context,
        PreparationOutcome::Committed(receipt) => return Ok(receipt),
        PreparationOutcome::RolledBack(receipt) => {
            return Err(ManagedVersionBundlePublicationError::Effect(Box::new(
                receipt,
            )));
        }
    };

    tokio::spawn(async move { run_publication_blocking(move || mutate(context)).await })
        .await
        .map_err(|_| ManagedVersionBundlePublicationError::TaskStopped)?
        .map_err(|_| ManagedVersionBundlePublicationError::TaskStopped)?
        .map_err(|receipt| ManagedVersionBundlePublicationError::Effect(Box::new(receipt)))
}

fn prepare_transaction(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    version_id: String,
    fingerprints: Vec<EntryFingerprint>,
    #[cfg(test)] test_hook: Option<PublicationTestHook>,
) -> Result<PreparationOutcome, ManagedVersionBundlePublicationError> {
    let mut planned = bind_sources(source, fingerprints)?;
    let lane = open_lane(&lease)?;
    recover_settled_lane(&lease, &lane)?;

    if let Some((intent, intent_guard)) = read_intent(&lane)? {
        let current_root_binding = lease
            .root()
            .identity()
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .persistent_binding();
        if intent.root_binding != current_root_binding {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
        if !intent_matches_projection(&intent, &version_id, &planned)? {
            return Err(ManagedVersionBundlePublicationError::LaneOccupied);
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
                #[cfg(test)]
                test_hook,
            )?;
            return match terminal {
                PersistedTerminalOutcome::Committed => {
                    Ok(PreparationOutcome::Committed(committed_receipt(context)))
                }
                PersistedTerminalOutcome::RolledBack { effect } => Ok(
                    PreparationOutcome::RolledBack(ManagedVersionBundleFailureReceipt {
                        context: Arc::new(context),
                        effect,
                        expectation: SettlementExpectation::Proven(
                            PersistedTerminalOutcome::RolledBack { effect },
                        ),
                    }),
                ),
            };
        }
        if recover_unfinished_commit(&lease, &lane, &staging, &quarantine, &intent, &planned)? {
            let (outcome, outcome_guard) = read_outcome(&lane)?
                .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
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
                #[cfg(test)]
                test_hook,
            )?;
            return Ok(PreparationOutcome::Committed(committed_receipt(context)));
        }
        let root_identity = lease
            .root()
            .identity()
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
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
            #[cfg(test)]
            test_hook,
        )?;
        return Ok(PreparationOutcome::Ready(Box::new(context)));
    }
    if read_outcome(&lane)?.is_some() {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
    let root_binding = lease
        .root()
        .identity()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        .persistent_binding();
    let intent = persisted_intent(&version_id, &root_binding, &planned, created_ancestors)?;
    let intent_bytes = marker_bytes(&intent)?;
    lane.write_new_exact(INTENT_NAME, &intent_bytes)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lease
        .publication_directory()
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lease
        .root()
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    let intent_guard = lane
        .inspect_regular_file(INTENT_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        .ok_or(ManagedVersionBundlePublicationError::Preparation)?;
    let (staging, quarantine) = open_or_create_slots_after_intent(&lease, &lane)?;
    let root_identity = lease
        .root()
        .identity()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
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
        #[cfg(test)]
        test_hook,
    )
    .map(Box::new)
    .map(PreparationOutcome::Ready)
}

fn bind_sources(
    source: AuthenticatedVersionBundleSource,
    fingerprints: Vec<EntryFingerprint>,
) -> Result<Vec<PlannedEntry>, ManagedVersionBundlePublicationError> {
    let mut sources = source.into_sources();
    let mut planned = Vec::with_capacity(fingerprints.len());
    for fingerprint in fingerprints {
        let source_index = sources
            .iter()
            .position(|source| source.kind() == fingerprint.kind)
            .ok_or(ManagedVersionBundlePublicationError::ProjectionMismatch)?;
        planned.push(PlannedEntry {
            fingerprint,
            source: sources.remove(source_index),
            target: None,
        });
    }
    if !sources.is_empty() || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&planned.len()) {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    Ok(planned)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedIntent {
    schema: String,
    phase: PersistedIntentPhase,
    version_id: String,
    root_binding: String,
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
    RolledBack { effect: ManagedVersionBundleEffect },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedSettlement {
    schema: String,
    phase: PersistedSettlementPhase,
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
) -> Result<Vec<EntryFingerprint>, ManagedVersionBundlePublicationError> {
    if projection.component() != ManagedKnownGoodComponent::VersionBundle
        || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&projection.entry_count())
        || projection.expected_content_byte_count() > MAX_TIER2_AGGREGATE_BYTES
    {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
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
                    return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
                }
            };
            let (digest, size) = match entry.integrity() {
                KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size } => {
                    (digest.as_str().to_string(), *size)
                }
                KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
                    return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
                }
            };
            if size == 0 || size > MAX_TIER2_ARTIFACT_BYTES || !valid_sha1(&digest) {
                return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
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
) -> Result<(), ManagedVersionBundlePublicationError> {
    let mut paths = BTreeSet::new();
    for fingerprint in fingerprints {
        let folded_path = portable_fold(fingerprint.path.as_str());
        let portable = format!("{}/{folded_path}", fingerprint.root.directory_name());
        if !paths.insert(portable) {
            return Err(ManagedVersionBundlePublicationError::PortablePathAlias);
        }
    }
    Ok(())
}

fn validate_bundle_topology(
    version_id: &str,
    fingerprints: &[EntryFingerprint],
) -> Result<(), ManagedVersionBundlePublicationError> {
    let safe_version = KnownGoodRelativePath::new(version_id)
        .map_err(|_| ManagedVersionBundlePublicationError::ProjectionMismatch)?;
    if safe_version.as_str().contains('/') || fingerprints.len() < 2 || fingerprints.len() > 3 {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    let mut ordinals = BTreeSet::new();
    let mut metadata = 0;
    let mut client = 0;
    let mut log = 0;
    for entry in fingerprints {
        if !ordinals.insert(entry.ordinal) {
            return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
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
                    return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
                }
                log += 1;
            }
            _ => return Err(ManagedVersionBundlePublicationError::ProjectionMismatch),
        }
    }
    if metadata != 1 || client != 1 || log > 1 {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    Ok(())
}

fn persisted_intent(
    version_id: &str,
    root_binding: &str,
    planned: &[PlannedEntry],
    created_ancestors: Vec<String>,
) -> Result<PersistedIntent, ManagedVersionBundlePublicationError> {
    let intent = PersistedIntent {
        schema: INTENT_SCHEMA.to_string(),
        phase: PersistedIntentPhase::Prepared,
        version_id: version_id.to_string(),
        root_binding: root_binding.to_string(),
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
) -> Result<Vec<EntryFingerprint>, ManagedVersionBundlePublicationError> {
    if intent.schema != INTENT_SCHEMA
        || intent.phase != PersistedIntentPhase::Prepared
        || !valid_nonce(&intent.transaction_nonce)
        || !valid_root_binding(&intent.root_binding)
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    let safe_version = KnownGoodRelativePath::new(&intent.version_id)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if safe_version.as_str().contains('/')
        || !(2..=MAX_VERSION_BUNDLE_ENTRIES).contains(&intent.entries.len())
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
        let path = KnownGoodRelativePath::new(&entry.relative_path)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        match &entry.prior {
            PriorFingerprint::Absent => {}
            PriorFingerprint::ExistingFile { sha1, size }
                if *size <= MAX_TIER2_ARTIFACT_BYTES && valid_sha1(sha1) =>
            {
                total_prior = total_prior
                    .checked_add(*size)
                    .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            }
            PriorFingerprint::ExistingFile { .. } => {
                return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
            }
        }
        total_source = total_source
            .checked_add(entry.source_size)
            .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
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
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    validate_portable_aliases(&fingerprints)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    validate_bundle_topology(&intent.version_id, &fingerprints)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    validate_created_ancestors(intent, &fingerprints)?;
    Ok(fingerprints)
}

fn validate_created_ancestors(
    intent: &PersistedIntent,
    fingerprints: &[EntryFingerprint],
) -> Result<(), ManagedVersionBundlePublicationError> {
    let allowed = fingerprints
        .iter()
        .flat_map(ancestor_paths)
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for ancestor in &intent.created_ancestors {
        KnownGoodRelativePath::new(ancestor)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        if !allowed.contains(ancestor) || !observed.insert(ancestor.clone()) {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
    }
    if observed.into_iter().collect::<Vec<_>>() != intent.created_ancestors {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
    planned: &[PlannedEntry],
) -> Result<bool, ManagedVersionBundlePublicationError> {
    let persisted = validate_persisted_intent(intent)?;
    Ok(intent.version_id == version_id
        && persisted
            == planned
                .iter()
                .map(|entry| entry.fingerprint.clone())
                .collect::<Vec<_>>())
}

fn valid_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn valid_root_binding(value: &str) -> bool {
    let mut fields = value.split(':');
    fields.next() == Some("unix")
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_none()
}

#[cfg(windows)]
fn valid_root_binding(value: &str) -> bool {
    let mut fields = value.split(':');
    fields.next() == Some("windows")
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        && fields.next().is_none()
}

fn valid_fixed_hex_16(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn portable_fold(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn marker_bytes<T: Serialize>(marker: &T) -> Result<Vec<u8>, ManagedVersionBundlePublicationError> {
    let bytes = serde_json::to_vec(marker)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    if bytes.len() > MAX_MARKER_BYTES {
        return Err(ManagedVersionBundlePublicationError::Preparation);
    }
    Ok(bytes)
}

fn open_lane(
    lease: &ManagedRootPublicationLease,
) -> Result<ManagedDir, ManagedVersionBundlePublicationError> {
    lease
        .revalidate()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    let publication = lease.publication_directory();
    let lane_existed = publication
        .has_portably_exact_child_name(LANE_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let lane = if lane_existed {
        publication
            .open_child(LANE_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    } else {
        let lane = publication
            .open_or_create_child(LANE_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        publication
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        lease
            .root()
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        lane
    };
    lane.sweep_orphan_temps()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
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
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
                .entries_bounded(1)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
                .is_empty()
            && lane
                .open_child(QUARANTINE_NAME)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
                .entries_bounded(1)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
                .is_empty();
        if !names.is_empty() && !clean_reserved {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
    }
    Ok(lane)
}

fn open_or_create_slots_after_intent(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
) -> Result<(ManagedDir, ManagedDir), ManagedVersionBundlePublicationError> {
    let staging_exists = lane
        .has_portably_exact_child_name(STAGING_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let quarantine_exists = lane
        .has_portably_exact_child_name(QUARANTINE_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let staging = if staging_exists {
        lane.open_child(STAGING_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    } else {
        lane.open_or_create_child(STAGING_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
    };
    let quarantine = if quarantine_exists {
        lane.open_child(QUARANTINE_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    } else {
        lane.open_or_create_child(QUARANTINE_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
    };
    staging
        .sweep_orphan_temps()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    quarantine
        .sweep_orphan_temps()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if !staging_exists || !quarantine_exists {
        staging
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        quarantine
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        lane.sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        lease
            .publication_directory()
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        lease
            .root()
            .sync()
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    }
    Ok((staging, quarantine))
}

fn exact_names(
    directory: &ManagedDir,
    allowed: &[&str],
    max_entries: usize,
) -> Result<BTreeSet<String>, ManagedVersionBundlePublicationError> {
    let entries = directory
        .entries_bounded(max_entries + 1)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if entries.len() > max_entries {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    let allowed_folded = allowed
        .iter()
        .map(|name| (portable_fold(name), *name))
        .collect::<Vec<_>>();
    let mut names = BTreeSet::new();
    let mut folded = BTreeSet::new();
    for entry in entries {
        let entry = entry
            .to_str()
            .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        let entry_folded = portable_fold(entry);
        let Some((_, exact)) = allowed_folded
            .iter()
            .find(|(allowed, _)| allowed == &entry_folded)
        else {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        };
        if entry != *exact || !folded.insert(entry_folded) {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
        names.insert(entry.to_string());
    }
    Ok(names)
}

fn read_intent(
    lane: &ManagedDir,
) -> Result<Option<(PersistedIntent, ManagedFileGuard)>, ManagedVersionBundlePublicationError> {
    read_marker(lane, INTENT_NAME)
}

fn read_outcome(
    lane: &ManagedDir,
) -> Result<Option<(PersistedOutcome, ManagedFileGuard)>, ManagedVersionBundlePublicationError> {
    read_marker(lane, OUTCOME_NAME)
}

fn read_settlement(
    lane: &ManagedDir,
) -> Result<Option<(PersistedSettlement, ManagedFileGuard)>, ManagedVersionBundlePublicationError> {
    read_marker(lane, SETTLEMENT_NAME)
}

fn read_marker<T: DeserializeOwned + Serialize>(
    lane: &ManagedDir,
    name: &str,
) -> Result<Option<(T, ManagedFileGuard)>, ManagedVersionBundlePublicationError> {
    let Some(guard) = lane
        .inspect_regular_file(name)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    else {
        return Ok(None);
    };
    if guard.size() == 0 || guard.size() > MAX_MARKER_BYTES as u64 {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    let bytes = lane
        .read_guarded_file_bounded(name, &guard, MAX_MARKER_BYTES as u64)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let marker = serde_json::from_slice(&bytes)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if serde_json::to_vec(&marker)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        != bytes
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    Ok(Some((marker, guard)))
}

fn validate_outcome(
    outcome: &PersistedOutcome,
    intent: &PersistedIntent,
) -> Result<(), ManagedVersionBundlePublicationError> {
    if outcome.schema != OUTCOME_SCHEMA || outcome.transaction_nonce != intent.transaction_nonce {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    Ok(())
}

fn validate_settlement(
    settlement: &PersistedSettlement,
) -> Result<(), ManagedVersionBundlePublicationError> {
    if settlement.schema != SETTLEMENT_SCHEMA
        || settlement.phase != PersistedSettlementPhase::CallerSettled
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    validate_persisted_intent(&settlement.intent)?;
    validate_outcome(&settlement.outcome, &settlement.intent)
}

fn require_empty_lane(lane: &ManagedDir) -> Result<(), ManagedVersionBundlePublicationError> {
    let names = exact_names(lane, &[STAGING_NAME, QUARANTINE_NAME], 2)?;
    if names.is_empty() {
        return Ok(());
    }
    if names.len() != 2
        || !lane
            .open_child(STAGING_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .entries_bounded(1)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .is_empty()
        || !lane
            .open_child(QUARANTINE_NAME)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .entries_bounded(1)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .is_empty()
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    Ok(())
}

fn validate_existing_portable_paths(
    root: &ManagedDir,
    fingerprints: &[EntryFingerprint],
) -> Result<(), ManagedVersionBundlePublicationError> {
    for fingerprint in fingerprints {
        let root_name = fingerprint.root.directory_name();
        if !root
            .has_portably_exact_child_name(root_name)
            .map_err(|_| ManagedVersionBundlePublicationError::PortablePathAlias)?
        {
            continue;
        }
        let mut directory = root
            .open_child(root_name)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        let mut segments = fingerprint.path.as_str().split('/').peekable();
        while let Some(segment) = segments.next() {
            let exists = directory
                .has_portably_exact_child_name(segment)
                .map_err(|_| ManagedVersionBundlePublicationError::PortablePathAlias)?;
            if !exists {
                break;
            }
            if segments.peek().is_some() {
                directory = directory
                    .open_child(segment)
                    .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
            }
        }
    }
    Ok(())
}

fn preflight_canonical_targets(
    root: &ManagedDir,
    fingerprints: &[EntryFingerprint],
) -> Result<(Vec<Option<CanonicalTarget>>, Vec<String>), ManagedVersionBundlePublicationError> {
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
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        let prior_fingerprint = match previous.as_ref() {
            Some(previous) => {
                if previous.size() > MAX_TIER2_ARTIFACT_BYTES {
                    return Err(ManagedVersionBundlePublicationError::Preparation);
                }
                displaced_bytes = displaced_bytes
                    .checked_add(previous.size())
                    .ok_or(ManagedVersionBundlePublicationError::Preparation)?;
                if displaced_bytes > MAX_TIER2_AGGREGATE_BYTES {
                    return Err(ManagedVersionBundlePublicationError::Preparation);
                }
                PriorFingerprint::ExistingFile {
                    sha1: parent
                        .sha1_guarded_file(&name, previous, MAX_TIER2_ARTIFACT_BYTES)
                        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?,
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
) -> Result<Option<(ManagedDir, String)>, ManagedVersionBundlePublicationError> {
    let root_name = fingerprint.root.directory_name();
    if !root
        .has_portably_exact_child_name(root_name)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    {
        return Ok(None);
    }
    let mut directory = root
        .open_child(root_name)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let mut segments = fingerprint.path.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            directory
                .has_portably_exact_child_name(segment)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            return Ok(Some((directory, segment.to_string())));
        }
        if !directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        {
            return Ok(None);
        }
        directory = directory
            .open_child(segment)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    }
    Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous)
}

fn managed_directory_path_exists(
    root: &ManagedDir,
    path: &str,
) -> Result<bool, ManagedVersionBundlePublicationError> {
    let mut segments = path.split('/');
    let first = segments
        .next()
        .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if !root
        .has_portably_exact_child_name(first)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    {
        return Ok(false);
    }
    let mut directory = root
        .open_child(first)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    for segment in segments {
        if !directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        {
            return Ok(false);
        }
        directory = directory
            .open_child(segment)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    }
    Ok(true)
}

fn context_from_prepared(
    handles: TransactionHandles,
    root_identity: ManagedDirectoryIdentity,
    planned: &mut [PlannedEntry],
    #[cfg(test)] test_hook: Option<PublicationTestHook>,
) -> Result<TransactionContext, ManagedVersionBundlePublicationError> {
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
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        .is_empty()
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    let targets = rolled_back_targets(lease.root(), &intent)?;
    let mut entries = Vec::with_capacity(planned.len());
    for ((planned, persisted), target) in planned.iter().zip(&intent.entries).zip(targets) {
        let stage_guard = match staging
            .inspect_regular_file(&persisted.staging_slot)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        {
            Some(guard) => {
                authenticate_guarded(
                    &staging,
                    &persisted.staging_slot,
                    &guard,
                    &planned.fingerprint,
                )?;
                guard
            }
            None => {
                staging
                    .write_new_exact(&persisted.staging_slot, planned.source.bytes())
                    .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
                staging
                    .verify_authenticated(
                        &persisted.staging_slot,
                        planned.fingerprint.size,
                        &planned.fingerprint.digest,
                    )
                    .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
                staging
                    .inspect_regular_file(&persisted.staging_slot)
                    .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
                    .ok_or(ManagedVersionBundlePublicationError::Preparation)?
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
    staging
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    if !lane
        .file_guard_matches(INTENT_NAME, &intent_guard)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    lease
        .revalidate()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
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
        #[cfg(test)]
        test_hook,
    })
}

fn validate_slot_topology(
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<(), ManagedVersionBundlePublicationError> {
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
) -> Result<Vec<Option<CanonicalTarget>>, ManagedVersionBundlePublicationError> {
    let fingerprints = validate_persisted_intent(intent)?;
    let mut targets = Vec::with_capacity(fingerprints.len());
    for (fingerprint, persisted) in fingerprints.iter().zip(&intent.entries) {
        let Some((parent, name)) = open_canonical_parent(root, fingerprint)? else {
            if persisted.prior != PriorFingerprint::Absent {
                return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
            }
            targets.push(None);
            continue;
        };
        let previous = parent
            .inspect_regular_file(&name)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        match (&persisted.prior, previous.as_ref()) {
            (PriorFingerprint::Absent, None) => {}
            (PriorFingerprint::ExistingFile { sha1, size }, Some(guard)) => {
                authenticate_prior(&parent, &name, guard, sha1, *size)?;
            }
            _ => return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous),
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

fn authenticate_guarded(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    fingerprint: &EntryFingerprint,
) -> Result<(), ManagedVersionBundlePublicationError> {
    if guard.size() != fingerprint.size
        || directory
            .sha1_guarded_file(name, guard, MAX_TIER2_ARTIFACT_BYTES)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            != fingerprint.digest
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    Ok(())
}

fn authenticate_prior(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    sha1: &str,
    size: u64,
) -> Result<(), ManagedVersionBundlePublicationError> {
    if guard.size() != size
        || directory
            .sha1_guarded_file(name, guard, MAX_TIER2_ARTIFACT_BYTES)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            != sha1
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    Ok(())
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
    let mut materialized = Vec::with_capacity(intent.created_ancestors.len());
    for relative in &intent.created_ancestors {
        let mut directory = root.clone();
        for segment in relative.split('/') {
            if directory.has_portably_exact_child_name(segment)? {
                directory = directory.open_child(segment)?;
            } else {
                let parent = directory.clone();
                directory = parent.open_or_create_child(segment)?;
                directory.sync()?;
                parent.sync()?;
            }
        }
        materialized.push(directory);
    }
    for directory in materialized.iter().rev() {
        directory.sync()?;
    }
    root.sync()
}

fn sync_recorded_ancestors_bottom_up(
    root: &ManagedDir,
    intent: &PersistedIntent,
) -> Result<(), LoaderError> {
    let mut directories = Vec::with_capacity(intent.created_ancestors.len());
    for relative in &intent.created_ancestors {
        let mut directory = root.clone();
        for segment in relative.split('/') {
            directory = directory.open_child(segment)?;
        }
        directories.push(directory);
    }
    for directory in directories.iter().rev() {
        directory.sync()?;
    }
    root.sync()
}

fn open_canonical_parent_loader(
    root: &ManagedDir,
    fingerprint: &EntryFingerprint,
) -> Result<Option<(ManagedDir, String)>, LoaderError> {
    let root_name = fingerprint.root.directory_name();
    if !root.has_portably_exact_child_name(root_name)? {
        return Ok(None);
    }
    let mut directory = root.open_child(root_name)?;
    let mut segments = fingerprint.path.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            directory.has_portably_exact_child_name(segment)?;
            return Ok(Some((directory, segment.to_string())));
        }
        if !directory.has_portably_exact_child_name(segment)? {
            return Ok(None);
        }
        directory = directory.open_child(segment)?;
    }
    Err(LoaderError::Verify(
        "version bundle path has no file name".to_string(),
    ))
}

enum ObservedCanonical {
    Absent,
    Source(ManagedFileGuard),
    Prior(ManagedFileGuard),
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
) -> Result<RecoveryObservation, ManagedVersionBundlePublicationError> {
    let (parent, name, canonical_guard) = match open_canonical_parent(root, fingerprint)? {
        Some((parent, name)) => {
            let guard = parent
                .inspect_regular_file(&name)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            (Some(parent), name, guard)
        }
        None => (
            None,
            fingerprint
                .path
                .as_str()
                .rsplit('/')
                .next()
                .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
                .to_string(),
            None,
        ),
    };
    let canonical = match (parent.as_ref(), canonical_guard) {
        (_, None) => ObservedCanonical::Absent,
        (Some(parent), Some(guard)) => {
            let digest = parent
                .sha1_guarded_file(&name, &guard, MAX_TIER2_ARTIFACT_BYTES)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            if guard.size() == fingerprint.size && digest == fingerprint.digest {
                ObservedCanonical::Source(guard)
            } else if matches!(
                &persisted.prior,
                PriorFingerprint::ExistingFile { sha1, size }
                    if *size == guard.size() && sha1 == &digest
            ) {
                ObservedCanonical::Prior(guard)
            } else {
                return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
            }
        }
        (None, Some(_)) => return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous),
    };
    let stage = staging
        .inspect_regular_file(&persisted.staging_slot)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    if let Some(stage) = stage.as_ref() {
        authenticate_guarded(staging, &persisted.staging_slot, stage, fingerprint)?;
    }
    let quarantined = quarantine
        .inspect_regular_file(&persisted.quarantine_slot)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    match (&persisted.prior, quarantined.as_ref()) {
        (_, None) => {}
        (PriorFingerprint::ExistingFile { sha1, size }, Some(guard)) => {
            authenticate_prior(quarantine, &persisted.quarantine_slot, guard, sha1, *size)?;
        }
        (PriorFingerprint::Absent, Some(_)) => {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
) -> Result<UnfinishedMoveOutcome, ManagedVersionBundlePublicationError> {
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
            committed_shape(observed, fingerprint, persisted)
        })
    {
        return Ok(UnfinishedMoveOutcome::Committed);
    }
    if !observations
        .iter()
        .zip(fingerprints.iter().zip(&intent.entries))
        .all(|(observed, (fingerprint, persisted))| {
            rollback_shape_is_reachable(observed, fingerprint, persisted)
        })
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }

    for index in (0..observations.len()).rev() {
        let observed = &mut observations[index];
        let persisted = &intent.entries[index];
        let fingerprint = &fingerprints[index];
        match &persisted.prior {
            PriorFingerprint::Absent => {
                if observed.quarantine.is_some() {
                    return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                }
                match &observed.canonical {
                    ObservedCanonical::Source(source) => {
                        if observed.stage.is_some() {
                            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                        }
                        let parent = observed
                            .parent
                            .as_ref()
                            .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                        parent
                            .rename_guarded_file_no_replace(
                                &observed.name,
                                source,
                                staging,
                                &persisted.staging_slot,
                            )
                            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                        parent
                            .sync()
                            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                        staging
                            .sync()
                            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    }
                    ObservedCanonical::Absent => {}
                    ObservedCanonical::Prior(_) => {
                        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                    }
                }
            }
            PriorFingerprint::ExistingFile { sha1, size }
                if persisted.prior.matches_source(fingerprint) =>
            {
                if observed.quarantine.is_some()
                    || !matches!(&observed.canonical, ObservedCanonical::Source(_))
                {
                    return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                }
                let parent = observed
                    .parent
                    .as_ref()
                    .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                let ObservedCanonical::Source(guard) = &observed.canonical else {
                    unreachable!("matched source state")
                };
                authenticate_prior(parent, &observed.name, guard, sha1, *size)?;
            }
            PriorFingerprint::ExistingFile { .. } => match &observed.canonical {
                ObservedCanonical::Source(source) => {
                    if observed.stage.is_some() {
                        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                    }
                    let prior = observed
                        .quarantine
                        .as_ref()
                        .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    let parent = observed
                        .parent
                        .as_ref()
                        .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    parent
                        .rename_guarded_file_no_replace(
                            &observed.name,
                            source,
                            staging,
                            &persisted.staging_slot,
                        )
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    parent
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    staging
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    quarantine
                        .rename_guarded_file_no_replace(
                            &persisted.quarantine_slot,
                            prior,
                            parent,
                            &observed.name,
                        )
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    quarantine
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    parent
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                }
                ObservedCanonical::Absent => {
                    let prior = observed
                        .quarantine
                        .as_ref()
                        .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    let parent = observed
                        .parent
                        .as_ref()
                        .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    quarantine
                        .rename_guarded_file_no_replace(
                            &persisted.quarantine_slot,
                            prior,
                            parent,
                            &observed.name,
                        )
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    quarantine
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                    parent
                        .sync()
                        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                }
                ObservedCanonical::Prior(_) => {
                    if observed.quarantine.is_some() {
                        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                    }
                }
            },
        }
    }
    lease
        .revalidate()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    Ok(UnfinishedMoveOutcome::RolledBack)
}

fn recover_unfinished_commit(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    staging: &ManagedDir,
    quarantine: &ManagedDir,
    intent: &PersistedIntent,
    planned: &[PlannedEntry],
) -> Result<bool, ManagedVersionBundlePublicationError> {
    if reconcile_unfinished_moves(lease, staging, quarantine, intent)?
        == UnfinishedMoveOutcome::Committed
    {
        write_outcome(lane, intent, PersistedTerminalOutcome::Committed)?;
        return Ok(true);
    }
    // Preparation can have stopped after intent but before every stage write. The
    // retry supplies the same authenticated projection and completes only missing slots.
    for (planned, persisted) in planned.iter().zip(&intent.entries) {
        if staging
            .inspect_regular_file(&persisted.staging_slot)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
            .is_none()
        {
            staging
                .write_new_exact(&persisted.staging_slot, planned.source.bytes())
                .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        }
        staging
            .verify_authenticated(
                &persisted.staging_slot,
                planned.fingerprint.size,
                &planned.fingerprint.digest,
            )
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    }
    staging
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    Ok(false)
}

fn committed_shape(
    observed: &RecoveryObservation,
    fingerprint: &EntryFingerprint,
    persisted: &PersistedEntry,
) -> bool {
    if !matches!(&observed.canonical, ObservedCanonical::Source(_)) {
        return false;
    }
    match &persisted.prior {
        PriorFingerprint::Absent => observed.stage.is_none() && observed.quarantine.is_none(),
        PriorFingerprint::ExistingFile { .. } if persisted.prior.matches_source(fingerprint) => {
            observed.stage.is_some() && observed.quarantine.is_none()
        }
        PriorFingerprint::ExistingFile { .. } => {
            observed.stage.is_none() && observed.quarantine.is_some()
        }
    }
}

fn rollback_shape_is_reachable(
    observed: &RecoveryObservation,
    fingerprint: &EntryFingerprint,
    persisted: &PersistedEntry,
) -> bool {
    match &persisted.prior {
        PriorFingerprint::Absent => {
            observed.quarantine.is_none()
                && match &observed.canonical {
                    ObservedCanonical::Source(_) => observed.stage.is_none(),
                    ObservedCanonical::Absent => true,
                    ObservedCanonical::Prior(_) => false,
                }
        }
        PriorFingerprint::ExistingFile { .. } if persisted.prior.matches_source(fingerprint) => {
            observed.quarantine.is_none()
                && matches!(&observed.canonical, ObservedCanonical::Source(_))
        }
        PriorFingerprint::ExistingFile { .. } => match &observed.canonical {
            ObservedCanonical::Source(_) => {
                observed.stage.is_none() && observed.quarantine.is_some()
            }
            ObservedCanonical::Absent => observed.stage.is_some() && observed.quarantine.is_some(),
            ObservedCanonical::Prior(_) => observed.quarantine.is_none(),
        },
    }
}

fn write_outcome(
    lane: &ManagedDir,
    intent: &PersistedIntent,
    outcome: PersistedTerminalOutcome,
) -> Result<ManagedFileGuard, ManagedVersionBundlePublicationError> {
    let marker = PersistedOutcome {
        schema: OUTCOME_SCHEMA.to_string(),
        transaction_nonce: intent.transaction_nonce.clone(),
        outcome,
    };
    lane.write_new_exact(OUTCOME_NAME, &marker_bytes(&marker)?)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lane.inspect_regular_file(OUTCOME_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        .ok_or(ManagedVersionBundlePublicationError::Preparation)
}

fn reconstruct_terminal_context(
    handles: TransactionHandles,
    outcome: PersistedOutcome,
    outcome_guard: ManagedFileGuard,
    #[cfg(test)] test_hook: Option<PublicationTestHook>,
) -> Result<TransactionContext, ManagedVersionBundlePublicationError> {
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
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
                if !committed_shape(
                    &RecoveryObservation {
                        parent: parent.clone(),
                        name: name.clone(),
                        canonical,
                        stage,
                        quarantine: quarantined,
                    },
                    &fingerprint,
                    persisted,
                ) {
                    return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
                    return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
                };
                let parent =
                    parent.ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
                let (state, previous) = match &persisted.prior {
                    PriorFingerprint::Absent => (EntryState::PublishedNew, None),
                    PriorFingerprint::ExistingFile { .. }
                        if persisted.prior.matches_source(&fingerprint) =>
                    {
                        (EntryState::AlreadyExact, None)
                    }
                    PriorFingerprint::ExistingFile { .. } => (
                        EntryState::PublishedReplacement,
                        Some(
                            quarantined
                                .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?,
                        ),
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
                    return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
                                .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?,
                            name,
                            previous: Some(guard),
                            prior_fingerprint: persisted.prior.clone(),
                        })
                    }
                    (PriorFingerprint::ExistingFile { .. }, ObservedCanonical::Source(guard))
                        if persisted.prior.matches_source(&fingerprint) =>
                    {
                        Some(CanonicalTarget {
                            parent: parent
                                .ok_or(ManagedVersionBundlePublicationError::RecoveryAmbiguous)?,
                            name,
                            previous: Some(guard),
                            prior_fingerprint: persisted.prior.clone(),
                        })
                    }
                    _ => return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous),
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
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
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
        #[cfg(test)]
        test_hook,
    };
    match outcome.outcome {
        PersistedTerminalOutcome::Committed => revalidate_committed(&context),
        PersistedTerminalOutcome::RolledBack { .. } => revalidate_failure(&context),
    }
    .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    Ok(context)
}

fn committed_receipt(context: TransactionContext) -> ManagedVersionBundleCommitReceipt {
    let dispositions = committed_dispositions(&context);
    ManagedVersionBundleCommitReceipt {
        context: Arc::new(context),
        dispositions,
    }
}

fn committed_dispositions(
    context: &TransactionContext,
) -> Vec<ManagedVersionBundleOrdinalDisposition> {
    context
        .entries
        .iter()
        .map(|entry| ManagedVersionBundleOrdinalDisposition {
            ordinal: entry.fingerprint.ordinal,
            disposition: match entry.state {
                EntryState::AlreadyExact => ManagedVersionBundleDisposition::AlreadyExact,
                EntryState::PublishedNew => ManagedVersionBundleDisposition::PublishedNew,
                EntryState::PublishedReplacement => {
                    ManagedVersionBundleDisposition::ReplacedWithQuarantine
                }
                EntryState::Prepared
                | EntryState::Quarantined
                | EntryState::RolledBack
                | EntryState::RollbackUncertain => unreachable!("terminal committed state"),
            },
        })
        .collect()
}

enum MutationDecision {
    Committed(Vec<ManagedVersionBundleOrdinalDisposition>),
    RolledBack { effect: ManagedVersionBundleEffect },
    Pending { effect: ManagedVersionBundleEffect },
}

fn mutate(
    mut context: TransactionContext,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleFailureReceipt> {
    let mut current_effect = ManagedVersionBundleEffect::Promotion;
    let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mutate_in_place(&mut context, &mut current_effect)
    }));
    match decision {
        Ok(MutationDecision::Committed(dispositions)) => Ok(ManagedVersionBundleCommitReceipt {
            context: Arc::new(context),
            dispositions,
        }),
        Ok(MutationDecision::RolledBack { effect }) => Err(terminal_failure(context, effect)),
        Ok(MutationDecision::Pending { effect }) => Err(reconciliation_failure(context, effect)),
        Err(_) => Err(reconciliation_failure(context, current_effect)),
    }
}

fn mutate_in_place(
    context: &mut TransactionContext,
    current_effect: &mut ManagedVersionBundleEffect,
) -> MutationDecision {
    *current_effect = ManagedVersionBundleEffect::Promotion;
    if prepare_canonical_targets(context).is_err() {
        return rollback_mutation_failure(
            context,
            ManagedVersionBundleEffect::Promotion,
            current_effect,
        );
    }
    for index in 0..context.entries.len() {
        if context.lease.revalidate().is_err() || promote_entry(context, index).is_err() {
            return rollback_mutation_failure(
                context,
                ManagedVersionBundleEffect::Promotion,
                current_effect,
            );
        }
        #[cfg(test)]
        if apply_test_hook(context, index + 1) {
            return rollback_mutation_failure(
                context,
                ManagedVersionBundleEffect::Promotion,
                current_effect,
            );
        }
    }
    *current_effect = ManagedVersionBundleEffect::Postcheck;
    if sync_recorded_ancestors_bottom_up(context.lease.root(), &context.intent).is_err()
        || verify_committed_physical(context).is_err()
    {
        return rollback_mutation_failure(
            context,
            ManagedVersionBundleEffect::Postcheck,
            current_effect,
        );
    }
    let outcome_guard = match write_outcome(
        &context.lane,
        &context.intent,
        PersistedTerminalOutcome::Committed,
    ) {
        Ok(guard) => guard,
        Err(_) => {
            return MutationDecision::Pending {
                effect: ManagedVersionBundleEffect::Postcheck,
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
            effect: ManagedVersionBundleEffect::Postcheck,
        };
    }
    if revalidate_committed(context).is_err() {
        context.outcome_guard = None;
        return MutationDecision::Pending {
            effect: ManagedVersionBundleEffect::Postcheck,
        };
    }
    MutationDecision::Committed(committed_dispositions(context))
}

fn rollback_mutation_failure(
    context: &mut TransactionContext,
    effect: ManagedVersionBundleEffect,
    current_effect: &mut ManagedVersionBundleEffect,
) -> MutationDecision {
    *current_effect = ManagedVersionBundleEffect::Rollback;
    if rollback(context).is_ok() {
        match write_outcome(
            &context.lane,
            &context.intent,
            PersistedTerminalOutcome::RolledBack { effect },
        ) {
            Ok(guard) => context.outcome_guard = Some(guard),
            Err(_) => {
                return MutationDecision::Pending {
                    effect: ManagedVersionBundleEffect::Rollback,
                };
            }
        }
        if revalidate_failure(context).is_ok() {
            return MutationDecision::RolledBack { effect };
        }
    }
    MutationDecision::Pending {
        effect: ManagedVersionBundleEffect::Rollback,
    }
}

fn promote_entry(context: &mut TransactionContext, index: usize) -> Result<(), LoaderError> {
    #[cfg(test)]
    let crash_after_quarantine = matches!(
        context.test_hook.as_ref(),
        Some(PublicationTestHook::CrashAfterQuarantine { kind })
            if *kind == context.entries[index].fingerprint.kind
    );
    let entry = &mut context.entries[index];
    let target = entry
        .target
        .as_mut()
        .ok_or_else(|| LoaderError::Verify("version bundle target was not prepared".to_string()))?;
    if target.prior_fingerprint.matches_source(&entry.fingerprint) {
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
            ));
        }
        entry.canonical_guard = target.parent.inspect_regular_file(&target.name)?;
        entry.state = EntryState::AlreadyExact;
        return Ok(());
    }
    if let Some(previous) = target.previous.as_ref() {
        let PriorFingerprint::ExistingFile { sha1, size } = &target.prior_fingerprint else {
            return Err(LoaderError::Verify(
                "version bundle prior guard lacks a fingerprint".to_string(),
            ));
        };
        if previous.size() != *size
            || target
                .parent
                .sha1_guarded_file(&target.name, previous, MAX_TIER2_ARTIFACT_BYTES)?
                != *sha1
        {
            return Err(LoaderError::Verify(
                "version bundle prior changed before quarantine".to_string(),
            ));
        }
        target.parent.rename_guarded_file_no_replace(
            &target.name,
            previous,
            &context.quarantine,
            &entry.quarantine_name,
        )?;
        entry.state = EntryState::Quarantined;
        target.parent.sync()?;
        context.quarantine.sync()?;
        context.lease.revalidate().map_err(publication_as_loader)?;
        #[cfg(test)]
        if crash_after_quarantine {
            panic!("injected version bundle crash after quarantine");
        }
    }
    let stage_guard = entry.stage_guard.as_ref().ok_or_else(|| {
        LoaderError::Verify("version bundle staged source guard is absent".to_string())
    })?;
    context.staging.rename_guarded_file_no_replace(
        &entry.stage_name,
        stage_guard,
        &target.parent,
        &target.name,
    )?;
    entry.canonical_guard = entry.stage_guard.take();
    entry.state = if target.previous.is_some() {
        EntryState::PublishedReplacement
    } else {
        EntryState::PublishedNew
    };
    context.staging.sync()?;
    target.parent.sync()?;
    context.lease.revalidate().map_err(publication_as_loader)?;
    target.parent.verify_authenticated(
        &target.name,
        entry.fingerprint.size,
        &entry.fingerprint.digest,
    )
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
            let Some(canonical_guard) = entry.canonical_guard.as_ref() else {
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
                || target.parent.sync().is_err()
                || context.staging.sync().is_err()
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
            let Some(previous) = target.previous.as_ref() else {
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
                || context.quarantine.sync().is_err()
                || target.parent.sync().is_err()
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
    if !context.lane.file_guard_matches(OUTCOME_NAME, &observed)?
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
    let (outcome, _) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle rollback outcome is absent".to_string())
        })?;
    if validate_outcome(&outcome, &context.intent).is_err()
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

#[cfg(test)]
fn apply_test_hook(context: &mut TransactionContext, promotions: usize) -> bool {
    let promoted_kind = promotions
        .checked_sub(1)
        .and_then(|index| context.entries.get(index))
        .map(|entry| entry.fingerprint.kind);
    match context.test_hook.as_mut() {
        Some(PublicationTestHook::FailAfter {
            promotions: expected,
        }) if *expected == promotions => true,
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
        Some(PublicationTestHook::CrashAfterPromotion { kind }) if Some(*kind) == promoted_kind => {
            panic!("injected version bundle crash after promotion");
        }
        Some(
            PublicationTestHook::FailAfter { .. }
            | PublicationTestHook::PauseAfter { .. }
            | PublicationTestHook::CrashAfterPromotion { .. }
            | PublicationTestHook::CrashAfterQuarantine { .. }
            | PublicationTestHook::FailSettlementOnce
            | PublicationTestHook::FailAfterSettlementMarkerOnce
            | PublicationTestHook::FailAfterCommittedOutcomeOnce,
        )
        | None => false,
    }
}

#[cfg(test)]
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
pub(crate) fn crash_after_artifact_quarantine_for_test(
    version_id: &str,
    kind: KnownGoodArtifactKind,
) {
    TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            version_id.to_string(),
            PublicationTestHook::CrashAfterQuarantine { kind },
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
) -> Result<ManagedVersionBundleSettlementOutcome, ManagedVersionBundleSettlementFailure> {
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
            drop(context);
            Ok(outcome.into())
        }
        None => Err(ManagedVersionBundleSettlementFailure {
            context: Arc::new(context),
            expectation: retained_expectation,
        }),
    }
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
        cleanup_settled_lane(
            &context.lease,
            &context.lane,
            &settlement,
            &settlement_guard,
        )
        .map_err(publication_error_as_loader)?;
        return Ok(settlement.outcome.outcome);
    }
    let lane_names = context.lane.entries_bounded(MAX_LANE_ENTRIES + 1)?;
    if matches!(*expectation, SettlementExpectation::Proven(_))
        && lane_names.len() == 2
        && context.staging.entries_bounded(1)?.is_empty()
        && context.quarantine.entries_bounded(1)?.is_empty()
        && context.lane.has_portably_exact_child_name(STAGING_NAME)?
        && context
            .lane
            .has_portably_exact_child_name(QUARANTINE_NAME)?
    {
        let SettlementExpectation::Proven(expected_outcome) = *expectation else {
            unreachable!("marker-free settlement requires a proven outcome")
        };
        validate_marker_free_settlement_shape(context, expected_outcome)?;
        context.staging.sync()?;
        context.quarantine.sync()?;
        context.lane.sync()?;
        context.lease.publication_directory().sync()?;
        context.lease.root().sync()?;
        return Ok(expected_outcome);
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
    let outcome = PersistedOutcome {
        schema: OUTCOME_SCHEMA.to_string(),
        transaction_nonce: context.intent.transaction_nonce.clone(),
        outcome: expected_outcome,
    };
    let settlement = PersistedSettlement {
        schema: SETTLEMENT_SCHEMA.to_string(),
        phase: PersistedSettlementPhase::CallerSettled,
        intent: context.intent.clone(),
        outcome,
    };
    context.lane.write_new_exact(
        SETTLEMENT_NAME,
        &marker_bytes(&settlement).map_err(publication_error_as_loader)?,
    )?;
    context.lane.sync()?;
    context.lease.publication_directory().sync()?;
    context.lease.root().sync()?;
    let settlement_guard = context
        .lane
        .inspect_regular_file(SETTLEMENT_NAME)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle settlement marker is absent".to_string())
        })?;
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
    cleanup_settled_lane(
        &context.lease,
        &context.lane,
        &settlement,
        &settlement_guard,
    )
    .map_err(publication_error_as_loader)?;
    Ok(expected_outcome)
}

fn validate_proven_outcome(
    context: &TransactionContext,
    expected_outcome: PersistedTerminalOutcome,
) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || context.lease.root().identity()?.persistent_binding() != context.intent.root_binding
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
    effect: ManagedVersionBundleEffect,
) -> Result<PersistedTerminalOutcome, LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || context.lease.root().identity()?.persistent_binding() != context.intent.root_binding
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
        validate_outcome(&outcome, &context.intent).map_err(publication_error_as_loader)?;
        validate_exact_terminal_shape(context, outcome.outcome)?;
        context.outcome_guard = Some(guard);
        return Ok(outcome.outcome);
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
    let _ = write_outcome(&context.lane, &context.intent, outcome)
        .map_err(publication_error_as_loader)?;
    let (persisted, guard) = read_outcome(&context.lane)
        .map_err(publication_error_as_loader)?
        .ok_or_else(|| {
            LoaderError::Verify("version bundle reconciled outcome is absent".to_string())
        })?;
    validate_outcome(&persisted, &context.intent).map_err(publication_error_as_loader)?;
    if persisted.outcome != outcome {
        return Err(LoaderError::Verify(
            "version bundle reconciled outcome changed".to_string(),
        ));
    }
    context.outcome_guard = Some(guard);
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
            PersistedTerminalOutcome::Committed => {
                committed_shape(&observed, fingerprint, persisted)
            }
            PersistedTerminalOutcome::RolledBack { .. } => {
                observed.stage.is_some()
                    && observed.quarantine.is_none()
                    && match &persisted.prior {
                        PriorFingerprint::Absent => {
                            matches!(&observed.canonical, ObservedCanonical::Absent)
                        }
                        PriorFingerprint::ExistingFile { .. }
                            if persisted.prior.matches_source(fingerprint) =>
                        {
                            matches!(&observed.canonical, ObservedCanonical::Source(_))
                        }
                        PriorFingerprint::ExistingFile { .. } => {
                            matches!(&observed.canonical, ObservedCanonical::Prior(_))
                        }
                    }
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

fn recover_settled_lane(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
) -> Result<(), ManagedVersionBundlePublicationError> {
    let Some((settlement, settlement_guard)) = read_settlement(lane)? else {
        return Ok(());
    };
    validate_settlement(&settlement)?;
    let current_root_binding = lease
        .root()
        .identity()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        .persistent_binding();
    if settlement.intent.root_binding != current_root_binding {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    cleanup_settled_lane(lease, lane, &settlement, &settlement_guard)
}

fn cleanup_settled_lane(
    lease: &ManagedRootPublicationLease,
    lane: &ManagedDir,
    settlement: &PersistedSettlement,
    settlement_guard: &ManagedFileGuard,
) -> Result<(), ManagedVersionBundlePublicationError> {
    validate_settlement(settlement)?;
    if lease
        .root()
        .identity()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
        .persistent_binding()
        != settlement.intent.root_binding
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
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
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    let staging = lane
        .open_child(STAGING_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    let quarantine = lane
        .open_child(QUARANTINE_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    validate_slot_topology(&staging, &quarantine, &settlement.intent)?;
    if let Some((intent, _)) = read_intent(lane)?
        && intent != settlement.intent
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    if let Some((outcome, _)) = read_outcome(lane)?
        && outcome != settlement.outcome
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
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
        if !settled_terminal_shape_is_valid(
            settlement.outcome.outcome,
            fingerprint,
            persisted,
            observed,
        ) {
            return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
        }
    }
    for (persisted, observed) in settlement.intent.entries.iter().zip(observations) {
        if let Some(stage) = observed.stage {
            staging
                .remove_guarded_file(&persisted.staging_slot, &stage)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            staging
                .sync()
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        }
        if let Some(quarantined) = observed.quarantine {
            quarantine
                .remove_guarded_file(&persisted.quarantine_slot, &quarantined)
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
            quarantine
                .sync()
                .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        }
    }
    if let Some((_, outcome_guard)) = read_outcome(lane)? {
        lane.remove_guarded_file(OUTCOME_NAME, &outcome_guard)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        lane.sync()
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    }
    if let Some((_, intent_guard)) = read_intent(lane)? {
        lane.remove_guarded_file(INTENT_NAME, &intent_guard)
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
        lane.sync()
            .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    }
    if !lane
        .file_guard_matches(SETTLEMENT_NAME, settlement_guard)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?
    {
        return Err(ManagedVersionBundlePublicationError::RecoveryAmbiguous);
    }
    lane.remove_guarded_file(SETTLEMENT_NAME, settlement_guard)
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    lease
        .publication_directory()
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    lease
        .root()
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::RecoveryAmbiguous)?;
    Ok(())
}

fn validate_marker_free_settlement_shape(
    context: &TransactionContext,
    expected_outcome: PersistedTerminalOutcome,
) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || context.lease.root().identity()?.persistent_binding() != context.intent.root_binding
    {
        return Err(LoaderError::Verify(
            "version bundle marker-free settlement root binding changed".to_string(),
        ));
    }
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
        if !settled_terminal_shape_is_valid(expected_outcome, fingerprint, persisted, &observed) {
            return Err(LoaderError::Verify(
                "version bundle marker-free settlement terminal shape changed".to_string(),
            ));
        }
    }
    Ok(())
}

fn settled_terminal_shape_is_valid(
    outcome: PersistedTerminalOutcome,
    fingerprint: &EntryFingerprint,
    persisted: &PersistedEntry,
    observed: &RecoveryObservation,
) -> bool {
    match outcome {
        PersistedTerminalOutcome::Committed => {
            matches!(&observed.canonical, ObservedCanonical::Source(_))
                && match &persisted.prior {
                    PriorFingerprint::Absent => observed.quarantine.is_none(),
                    PriorFingerprint::ExistingFile { .. }
                        if persisted.prior.matches_source(fingerprint) =>
                    {
                        observed.quarantine.is_none()
                    }
                    PriorFingerprint::ExistingFile { .. } => true,
                }
        }
        PersistedTerminalOutcome::RolledBack { .. } => {
            observed.quarantine.is_none()
                && match &persisted.prior {
                    PriorFingerprint::Absent => {
                        matches!(&observed.canonical, ObservedCanonical::Absent)
                    }
                    PriorFingerprint::ExistingFile { .. }
                        if persisted.prior.matches_source(fingerprint) =>
                    {
                        matches!(&observed.canonical, ObservedCanonical::Source(_))
                    }
                    PriorFingerprint::ExistingFile { .. } => {
                        matches!(&observed.canonical, ObservedCanonical::Prior(_))
                    }
                }
        }
    }
}

fn publication_as_loader(
    error: crate::managed_publication::ManagedPublicationError,
) -> LoaderError {
    LoaderError::Verify(error.to_string())
}

fn publication_error_as_loader(error: ManagedVersionBundlePublicationError) -> LoaderError {
    LoaderError::Verify(error.to_string())
}

fn terminal_failure(
    context: TransactionContext,
    effect: ManagedVersionBundleEffect,
) -> ManagedVersionBundleFailureReceipt {
    ManagedVersionBundleFailureReceipt {
        context: Arc::new(context),
        effect,
        expectation: SettlementExpectation::Proven(PersistedTerminalOutcome::RolledBack { effect }),
    }
}

fn reconciliation_failure(
    context: TransactionContext,
    effect: ManagedVersionBundleEffect,
) -> ManagedVersionBundleFailureReceipt {
    ManagedVersionBundleFailureReceipt {
        context: Arc::new(context),
        effect,
        expectation: SettlementExpectation::PendingFailure { effect },
    }
}

fn fingerprints(context: &TransactionContext) -> Vec<EntryFingerprint> {
    context
        .entries
        .iter()
        .map(|entry| entry.fingerprint.clone())
        .collect()
}

fn projection_matches_fingerprints(
    projection: &ManagedComponentProjection<'_>,
    expected: Vec<EntryFingerprint>,
) -> bool {
    own_fingerprints(projection).is_ok_and(|actual| actual == expected)
}

#[cfg(test)]
mod settlement_tests {
    use super::*;
    use sha1::{Digest as _, Sha1};

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
            root_binding: root_identity.persistent_binding(),
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
            &marker_bytes(&intent).expect("serialize settlement intent"),
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

    fn assert_settlement_cleaned(lane: &ManagedDir, staging: &ManagedDir, quarantine: &ManagedDir) {
        assert!(read_intent(lane).expect("read cleaned intent").is_none());
        assert!(read_outcome(lane).expect("read cleaned outcome").is_none());
        assert!(
            read_settlement(lane)
                .expect("read cleaned settlement")
                .is_none()
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
                effect: ManagedVersionBundleEffect::Rollback,
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

        assert_eq!(
            settlement.retry().await.expect("retry settlement"),
            ManagedVersionBundleSettlementOutcome::RolledBack {
                effect: ManagedVersionBundleEffect::Rollback,
            }
        );
        assert_settlement_cleaned(&lane, &staging, &quarantine);
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
                effect: ManagedVersionBundleEffect::Rollback,
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
        assert_eq!(
            settlement.retry().await.expect("resume marker cleanup"),
            ManagedVersionBundleSettlementOutcome::RolledBack {
                effect: ManagedVersionBundleEffect::Rollback,
            }
        );
        assert_settlement_cleaned(&lane, &staging, &quarantine);
    }
}
