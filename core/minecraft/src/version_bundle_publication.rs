use crate::download::AuthenticatedVersionBundleSource;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodRelativePath, KnownGoodRoot,
    MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection,
    ManagedKnownGoodComponent,
};
use crate::loaders::LoaderError;
use crate::managed_fs::{ManagedDir, ManagedDirectoryIdentity, ManagedFileGuard};
use crate::managed_publication::{ManagedRootPublicationLease, run_publication_blocking};
use serde::Serialize;
use std::collections::BTreeSet;
#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

const LANE_NAME: &str = "version-bundle";
const STAGING_NAME: &str = "staging";
const QUARANTINE_NAME: &str = "quarantine";
const MANIFEST_NAME: &str = "manifest.json";
const MAX_VERSION_BUNDLE_ENTRIES: usize = 3;
const MAX_LANE_ENTRIES: usize = 3;
const MAX_MANIFEST_BYTES: usize = 16 << 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFingerprint {
    ordinal: usize,
    root: PhysicalRoot,
    path: KnownGoodRelativePath,
    kind: KnownGoodArtifactKind,
    digest: String,
    size: u64,
}

struct TransactionEntry {
    fingerprint: EntryFingerprint,
    stage_name: String,
    quarantine_name: String,
    stage_guard: ManagedFileGuard,
    target: Option<CanonicalTarget>,
    state: EntryState,
}

struct CanonicalTarget {
    parent: ManagedDir,
    name: String,
    previous: Option<ManagedFileGuard>,
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
    manifest_guard: ManagedFileGuard,
    entries: Vec<TransactionEntry>,
    #[cfg(test)]
    test_hook: Option<PublicationTestHook>,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    #[error("version bundle publication lane is not empty")]
    LaneOccupied,
    #[error("version bundle publication preparation failed")]
    Preparation,
    #[error("version bundle publication task stopped unexpectedly")]
    TaskStopped,
    #[error("version bundle publication effects failed")]
    Effect(Box<ManagedVersionBundleFailureReceipt>),
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
        let context = Arc::clone(&self.context);
        run_publication_blocking(move || revalidate_failure(&context).is_ok())
            .await
            .is_ok_and(|valid| valid)
    }
}

pub(crate) async fn publish_version_bundle(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    projection: ManagedComponentProjection<'_>,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundlePublicationError> {
    if !source.matches_projection(&projection) {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    #[cfg(test)]
    let test_hook = TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(source.version_id());
    let fingerprints = own_fingerprints(&projection)?;
    validate_portable_aliases(&fingerprints)?;
    #[cfg(test)]
    let preparation = move || prepare_transaction(lease, source, fingerprints, test_hook);
    #[cfg(not(test))]
    let preparation = move || prepare_transaction(lease, source, fingerprints);
    let context = run_publication_blocking(preparation)
        .await
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)??;

    tokio::spawn(async move { run_publication_blocking(move || mutate(context)).await })
        .await
        .map_err(|_| ManagedVersionBundlePublicationError::TaskStopped)?
        .map_err(|_| ManagedVersionBundlePublicationError::TaskStopped)?
        .map_err(|receipt| ManagedVersionBundlePublicationError::Effect(Box::new(receipt)))
}

fn prepare_transaction(
    lease: ManagedRootPublicationLease,
    source: AuthenticatedVersionBundleSource,
    fingerprints: Vec<EntryFingerprint>,
    #[cfg(test)] test_hook: Option<PublicationTestHook>,
) -> Result<TransactionContext, ManagedVersionBundlePublicationError> {
    validate_existing_portable_paths(lease.root(), &fingerprints)?;
    let mut targets = preflight_canonical_targets(lease.root(), &fingerprints)?;
    let root_identity = lease
        .root()
        .identity()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    let publication = lease.publication_directory();
    let lane = publication
        .open_or_create_child(LANE_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    if !lane
        .entries_bounded(MAX_LANE_ENTRIES + 1)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        .is_empty()
    {
        return Err(ManagedVersionBundlePublicationError::LaneOccupied);
    }
    let staging = lane
        .open_or_create_child(STAGING_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    let quarantine = lane
        .open_or_create_child(QUARANTINE_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;

    let (version_json, client_jar, log_config) = source.into_sources();
    let mut sources = vec![version_json, client_jar];
    if let Some(log_config) = log_config {
        sources.push(log_config);
    }
    let mut entries = Vec::with_capacity(fingerprints.len());
    for (index, fingerprint) in fingerprints.into_iter().enumerate() {
        let source_index = sources
            .iter()
            .position(|source| source_matches_kind(source.kind(), fingerprint.kind))
            .ok_or(ManagedVersionBundlePublicationError::ProjectionMismatch)?;
        let source = sources.remove(source_index);
        let stage_name = format!("entry-{index}");
        let quarantine_name = format!("entry-{index}");
        staging
            .write_new_exact(&stage_name, source.bytes())
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        staging
            .verify_authenticated(&stage_name, fingerprint.size, &fingerprint.digest)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        let stage_guard = staging
            .inspect_regular_file(&stage_name)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
            .ok_or(ManagedVersionBundlePublicationError::Preparation)?;
        entries.push(TransactionEntry {
            fingerprint,
            stage_name,
            quarantine_name,
            stage_guard,
            target: targets.remove(0),
            state: EntryState::Prepared,
        });
    }
    if !sources.is_empty() || entries.len() < 2 || entries.len() > MAX_VERSION_BUNDLE_ENTRIES {
        return Err(ManagedVersionBundlePublicationError::ProjectionMismatch);
    }
    staging
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;

    let manifest = manifest_bytes(&entries)?;
    lane.write_new_exact(MANIFEST_NAME, &manifest)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    lane.sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    publication
        .sync()
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    let manifest_guard = lane
        .inspect_regular_file(MANIFEST_NAME)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        .ok_or(ManagedVersionBundlePublicationError::Preparation)?;
    Ok(TransactionContext {
        lease,
        root_identity,
        lane,
        staging,
        quarantine,
        manifest_guard,
        entries,
        #[cfg(test)]
        test_hook,
    })
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
        let folded_path = fingerprint
            .path
            .as_str()
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let portable = format!("{}/{folded_path}", fingerprint.root.directory_name());
        if !paths.insert(portable) {
            return Err(ManagedVersionBundlePublicationError::PortablePathAlias);
        }
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
            if !exists || segments.peek().is_none() {
                break;
            }
            directory = directory
                .open_child(segment)
                .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        }
    }
    Ok(())
}

fn preflight_canonical_targets(
    root: &ManagedDir,
    fingerprints: &[EntryFingerprint],
) -> Result<Vec<Option<CanonicalTarget>>, ManagedVersionBundlePublicationError> {
    let mut targets = Vec::with_capacity(fingerprints.len());
    let mut displaced_bytes = 0_u64;
    for fingerprint in fingerprints {
        let root_name = fingerprint.root.directory_name();
        if !root
            .has_portably_exact_child_name(root_name)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
        {
            targets.push(None);
            continue;
        }
        let mut directory = root
            .open_child(root_name)
            .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        let mut segments = fingerprint.path.as_str().split('/').peekable();
        let mut target = None;
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                let previous = directory
                    .inspect_regular_file(segment)
                    .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
                if let Some(previous) = previous.as_ref() {
                    if previous.size() > MAX_TIER2_ARTIFACT_BYTES {
                        return Err(ManagedVersionBundlePublicationError::Preparation);
                    }
                    displaced_bytes = displaced_bytes
                        .checked_add(previous.size())
                        .ok_or(ManagedVersionBundlePublicationError::Preparation)?;
                    if displaced_bytes > MAX_TIER2_AGGREGATE_BYTES {
                        return Err(ManagedVersionBundlePublicationError::Preparation);
                    }
                }
                target = Some(CanonicalTarget {
                    parent: directory,
                    name: segment.to_string(),
                    previous,
                });
                break;
            }
            if !directory
                .has_portably_exact_child_name(segment)
                .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?
            {
                break;
            }
            directory = directory
                .open_child(segment)
                .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
        }
        targets.push(target);
    }
    Ok(targets)
}

#[derive(Serialize)]
struct PersistedManifest<'a> {
    schema: &'static str,
    state: &'static str,
    entries: Vec<PersistedEntry<'a>>,
}

#[derive(Serialize)]
struct PersistedEntry<'a> {
    ordinal: usize,
    root: PhysicalRoot,
    relative_path: &'a str,
    kind: &'static str,
    sha1: &'a str,
    size: u64,
    prior: &'static str,
    prior_size: Option<u64>,
}

fn manifest_bytes(
    entries: &[TransactionEntry],
) -> Result<Vec<u8>, ManagedVersionBundlePublicationError> {
    let manifest = PersistedManifest {
        schema: "axial.version_bundle_publication.v1",
        state: "prepared",
        entries: entries
            .iter()
            .map(|entry| PersistedEntry {
                ordinal: entry.fingerprint.ordinal,
                root: entry.fingerprint.root,
                relative_path: entry.fingerprint.path.as_str(),
                kind: entry.fingerprint.kind.stable_id(),
                sha1: &entry.fingerprint.digest,
                size: entry.fingerprint.size,
                prior: if entry
                    .target
                    .as_ref()
                    .and_then(|target| target.previous.as_ref())
                    .is_some()
                {
                    "existing_file"
                } else {
                    "absent"
                },
                prior_size: entry
                    .target
                    .as_ref()
                    .and_then(|target| target.previous.as_ref())
                    .map(ManagedFileGuard::size),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|_| ManagedVersionBundlePublicationError::Preparation)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManagedVersionBundlePublicationError::Preparation);
    }
    Ok(bytes)
}

fn mutate(
    mut context: TransactionContext,
) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleFailureReceipt> {
    if prepare_canonical_targets(&mut context).is_err() {
        return Err(failure(context, ManagedVersionBundleEffect::Promotion));
    }
    for index in 0..context.entries.len() {
        if context.lease.revalidate().is_err() || promote_entry(&mut context, index).is_err() {
            let rollback_ok = rollback(&mut context).is_ok();
            return Err(failure(
                context,
                if rollback_ok {
                    ManagedVersionBundleEffect::Promotion
                } else {
                    ManagedVersionBundleEffect::Rollback
                },
            ));
        }
        #[cfg(test)]
        if apply_test_hook(&mut context, index + 1) {
            let rollback_ok = rollback(&mut context).is_ok();
            return Err(failure(
                context,
                if rollback_ok {
                    ManagedVersionBundleEffect::Promotion
                } else {
                    ManagedVersionBundleEffect::Rollback
                },
            ));
        }
    }
    if revalidate_committed(&context).is_err() {
        let rollback_ok = rollback(&mut context).is_ok();
        return Err(failure(
            context,
            if rollback_ok {
                ManagedVersionBundleEffect::Postcheck
            } else {
                ManagedVersionBundleEffect::Rollback
            },
        ));
    }
    let dispositions = context
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
                | EntryState::RollbackUncertain => unreachable!("committed state"),
            },
        })
        .collect();
    Ok(ManagedVersionBundleCommitReceipt {
        context: Arc::new(context),
        dispositions,
    })
}

#[cfg(test)]
fn apply_test_hook(context: &mut TransactionContext, promotions: usize) -> bool {
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
        Some(PublicationTestHook::FailAfter { .. } | PublicationTestHook::PauseAfter { .. })
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

fn prepare_canonical_targets(context: &mut TransactionContext) -> Result<(), LoaderError> {
    for entry in &mut context.entries {
        if entry.target.is_some() {
            continue;
        }
        let (parent, name) = materialize_parent(
            context.lease.root(),
            entry.fingerprint.root,
            &entry.fingerprint.path,
        )?;
        let previous = parent.inspect_regular_file(&name)?;
        if previous.is_some() {
            return Err(LoaderError::Verify(
                "version bundle target appeared after preflight".to_string(),
            ));
        }
        entry.target = Some(CanonicalTarget {
            parent,
            name,
            previous,
        });
    }
    context.lease.revalidate().map_err(publication_as_loader)
}

fn materialize_parent(
    root: &ManagedDir,
    physical_root: PhysicalRoot,
    relative: &KnownGoodRelativePath,
) -> Result<(ManagedDir, String), LoaderError> {
    let mut directory = root.open_or_create_child(physical_root.directory_name())?;
    let mut segments = relative.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return Ok((directory, segment.to_string()));
        }
        directory = directory.open_or_create_child(segment)?;
    }
    Err(LoaderError::Verify(
        "version bundle path has no file name".to_string(),
    ))
}

fn promote_entry(context: &mut TransactionContext, index: usize) -> Result<(), LoaderError> {
    let entry = &mut context.entries[index];
    let target = entry
        .target
        .as_mut()
        .ok_or_else(|| LoaderError::Verify("version bundle target was not prepared".to_string()))?;
    if target.previous.is_some()
        && target
            .parent
            .verify_authenticated(
                &target.name,
                entry.fingerprint.size,
                &entry.fingerprint.digest,
            )
            .is_ok()
    {
        entry.state = EntryState::AlreadyExact;
        return Ok(());
    }
    if let Some(previous) = target.previous.as_ref() {
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
    }
    context.staging.rename_guarded_file_no_replace(
        &entry.stage_name,
        &entry.stage_guard,
        &target.parent,
        &target.name,
    )?;
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
            continue;
        };
        if matches!(
            entry.state,
            EntryState::PublishedNew | EntryState::PublishedReplacement
        ) && (target
            .parent
            .rename_guarded_file_no_replace(
                &target.name,
                &entry.stage_guard,
                &context.staging,
                &entry.stage_name,
            )
            .is_err()
            || target.parent.sync().is_err()
            || context.staging.sync().is_err())
        {
            entry.state = EntryState::RollbackUncertain;
            complete = false;
            continue;
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
        if entry.state != EntryState::AlreadyExact {
            entry.state = EntryState::RolledBack;
        }
    }
    context.lease.revalidate().map_err(|_| ())?;
    complete.then_some(()).ok_or(())
}

fn revalidate_committed(context: &TransactionContext) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(MANIFEST_NAME, &context.manifest_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle receipt identity changed".to_string(),
        ));
    }
    for entry in &context.entries {
        let target = entry.target.as_ref().ok_or_else(|| {
            LoaderError::Verify("version bundle receipt target is absent".to_string())
        })?;
        let expected_guard = match entry.state {
            EntryState::AlreadyExact => target.previous.as_ref(),
            EntryState::PublishedNew | EntryState::PublishedReplacement => Some(&entry.stage_guard),
            EntryState::Prepared
            | EntryState::Quarantined
            | EntryState::RolledBack
            | EntryState::RollbackUncertain => None,
        }
        .ok_or_else(|| {
            LoaderError::Verify("version bundle receipt state is invalid".to_string())
        })?;
        if !target
            .parent
            .file_guard_matches(&target.name, expected_guard)?
        {
            return Err(LoaderError::Verify(
                "version bundle canonical identity changed".to_string(),
            ));
        }
        target.parent.verify_authenticated(
            &target.name,
            entry.fingerprint.size,
            &entry.fingerprint.digest,
        )?;
        if entry.state == EntryState::PublishedReplacement {
            let previous = target.previous.as_ref().ok_or_else(|| {
                LoaderError::Verify("version bundle quarantine guard is absent".to_string())
            })?;
            if !context
                .quarantine
                .file_guard_matches(&entry.quarantine_name, previous)?
            {
                return Err(LoaderError::Verify(
                    "version bundle quarantine identity changed".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn revalidate_failure(context: &TransactionContext) -> Result<(), LoaderError> {
    context.lease.revalidate().map_err(publication_as_loader)?;
    if context.lease.root().identity()? != context.root_identity
        || !context
            .lane
            .file_guard_matches(MANIFEST_NAME, &context.manifest_guard)?
    {
        return Err(LoaderError::Verify(
            "version bundle failure receipt identity changed".to_string(),
        ));
    }
    for entry in &context.entries {
        let target = entry.target.as_ref();
        match entry.state {
            EntryState::RolledBack => {
                if !context
                    .staging
                    .file_guard_matches(&entry.stage_name, &entry.stage_guard)?
                {
                    return Err(LoaderError::Verify(
                        "version bundle rollback stage changed".to_string(),
                    ));
                }
                if let (Some(target), Some(previous)) =
                    (target, target.and_then(|target| target.previous.as_ref()))
                    && !target.parent.file_guard_matches(&target.name, previous)?
                {
                    return Err(LoaderError::Verify(
                        "version bundle rollback target changed".to_string(),
                    ));
                }
            }
            EntryState::Prepared => {
                if !context
                    .staging
                    .file_guard_matches(&entry.stage_name, &entry.stage_guard)?
                {
                    return Err(LoaderError::Verify(
                        "version bundle prepared stage changed".to_string(),
                    ));
                }
            }
            EntryState::AlreadyExact => {}
            EntryState::Quarantined
            | EntryState::PublishedNew
            | EntryState::PublishedReplacement
            | EntryState::RollbackUncertain => {
                return Err(LoaderError::Verify(
                    "version bundle rollback remains uncertain".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn publication_as_loader(
    error: crate::managed_publication::ManagedPublicationError,
) -> LoaderError {
    LoaderError::Verify(error.to_string())
}

fn failure(
    context: TransactionContext,
    effect: ManagedVersionBundleEffect,
) -> ManagedVersionBundleFailureReceipt {
    ManagedVersionBundleFailureReceipt {
        context: Arc::new(context),
        effect,
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

fn source_matches_kind(
    source: crate::download::SelectedDownloadArtifactKind,
    artifact: KnownGoodArtifactKind,
) -> bool {
    matches!(
        (source, artifact),
        (
            crate::download::SelectedDownloadArtifactKind::VersionJson,
            KnownGoodArtifactKind::VersionMetadata
        ) | (
            crate::download::SelectedDownloadArtifactKind::ClientJar,
            KnownGoodArtifactKind::ClientJar
        ) | (
            crate::download::SelectedDownloadArtifactKind::LogConfig,
            KnownGoodArtifactKind::LogConfig
        )
    )
}
