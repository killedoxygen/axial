use super::cancellation::{
    RuntimeCancellation, RuntimeCancellationSet, RuntimeThreadCancellation,
    runtime_cancellation_channel,
};
#[cfg(test)]
use super::cancellation::{
    RuntimeTestGate, RuntimeTestHookPoint, arm_runtime_test_hook, wait_for_runtime_test_hook,
};
#[cfg(test)]
use super::file_download::runtime_filesystem_path;
use super::file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, RuntimeVerifiedSource,
    bounded_manifest_file_label, component_manifest_destination,
    component_manifest_destination_with_key, component_manifest_link_target_path,
    fetch_runtime_source_until_cancelled, runtime_file_download_concurrency,
    runtime_transfer_client, verify_runtime_download,
};
use super::layout::{ManagedRuntimeCache, managed_runtime_executable_ready};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, ComponentManifestDownload,
    ComponentManifestDownloads, ComponentManifestFile, RuntimeSourceReceipt,
    component_manifest_proof_bytes,
};
use super::model::{
    JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeId, RuntimeRecord, RuntimeSourceFailure,
    RuntimeSourceFailureKind,
};
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodInventory, KnownGoodRoot,
    known_good_link_target_matches,
};
use crate::managed_fs::{ManagedDir, ManagedDirectoryMoveFailure};
use crate::portable_path::{PortableFileName, PortablePathKey, PortableRelativePath};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const MAX_RUNTIME_TREE_ENTRIES: usize = 4096;
const MAX_RUNTIME_TREE_DEPTH: usize = 16;
const MAX_RUNTIME_LINK_TARGET_BYTES: usize = 4096;
const MAX_RUNTIME_FILE_BYTES: u64 = 128 << 20;
const MAX_RUNTIME_TREE_TOTAL_BYTES: u64 = 512 << 20;

fn runtime_source_failure(
    component: &RuntimeId,
    kind: RuntimeSourceFailureKind,
    detail: impl Into<String>,
) -> JavaRuntimeLookupError {
    JavaRuntimeLookupError::RuntimeSource(RuntimeSourceFailure::new(
        component.clone(),
        kind,
        detail,
    ))
}

pub(crate) struct StagedManagedRuntime {
    cache: ManagedRuntimeCache,
    component: RuntimeId,
    install_root: PathBuf,
    stage: OwnedRuntimeStage,
    source: Option<RuntimeSourceReceipt>,
    publication_lease: Option<ManagedRuntimePublicationLease>,
}

struct ManagedRuntimePublicationLease {
    _component_lock: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) struct VerifiedManagedRuntime {
    runtime: RuntimeRecord,
    source: RuntimeSourceReceipt,
    _publication_lease: ManagedRuntimePublicationLease,
}

pub(super) enum CachedManagedRuntimeVerification {
    Matched(VerifiedManagedRuntime),
    Mismatched(RuntimeSourceReceipt),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RuntimeTreeVerificationReason {
    StagePostMaterialization,
    PublicationPrePromotion,
    CanonicalReuse,
    PublicationPostPromotion,
    CachedSourceMatch,
    EnsureSourceMatch,
    ReceiptRevalidation,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeTreeVerificationCounts {
    pub(crate) stage_post_materialization: usize,
    pub(crate) publication_pre_promotion: usize,
    pub(crate) canonical_reuse: usize,
    pub(crate) publication_post_promotion: usize,
    pub(crate) cached_source_match: usize,
    pub(crate) ensure_source_match: usize,
    pub(crate) receipt_revalidation: usize,
}

#[cfg(test)]
impl RuntimeTreeVerificationCounts {
    pub(crate) fn total(self) -> usize {
        self.stage_post_materialization
            + self.publication_pre_promotion
            + self.canonical_reuse
            + self.publication_post_promotion
            + self.cached_source_match
            + self.ensure_source_match
            + self.receipt_revalidation
    }

    pub(crate) fn reason_vector(self) -> [usize; 7] {
        [
            self.stage_post_materialization,
            self.publication_pre_promotion,
            self.canonical_reuse,
            self.publication_post_promotion,
            self.cached_source_match,
            self.ensure_source_match,
            self.receipt_revalidation,
        ]
    }

    fn record(&mut self, reason: RuntimeTreeVerificationReason) {
        let count = match reason {
            RuntimeTreeVerificationReason::StagePostMaterialization => {
                &mut self.stage_post_materialization
            }
            RuntimeTreeVerificationReason::PublicationPrePromotion => {
                &mut self.publication_pre_promotion
            }
            RuntimeTreeVerificationReason::CanonicalReuse => &mut self.canonical_reuse,
            RuntimeTreeVerificationReason::PublicationPostPromotion => {
                &mut self.publication_post_promotion
            }
            RuntimeTreeVerificationReason::CachedSourceMatch => &mut self.cached_source_match,
            RuntimeTreeVerificationReason::EnsureSourceMatch => &mut self.ensure_source_match,
            RuntimeTreeVerificationReason::ReceiptRevalidation => &mut self.receipt_revalidation,
        };
        *count += 1;
    }
}

/// Sealed evidence that Core published and revalidated a managed runtime tree.
pub struct ManagedRuntimeCommitReceipt {
    cache: ManagedRuntimeCache,
    component: RuntimeId,
    source: Option<RuntimeSourceReceipt>,
    quarantine: Option<ManagedRuntimeQuarantineObligation>,
    _publication_lease: ManagedRuntimePublicationLease,
}

/// A durable, bounded obligation left when a canonical runtime was displaced.
pub struct ManagedRuntimeQuarantineObligation {
    cache: ManagedRuntimeCache,
    component: RuntimeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedRuntimeQuarantineObservation {
    Present,
    Absent,
    Indeterminate,
}

impl std::fmt::Debug for ManagedRuntimeCommitReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedRuntimeCommitReceipt { .. }")
    }
}

impl std::fmt::Debug for ManagedRuntimeFailureReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedRuntimeFailureReceipt { .. }")
    }
}

/// Failure evidence for an operation that had already changed managed publication state.
pub struct ManagedRuntimeFailureReceipt {
    cache: ManagedRuntimeCache,
    component: RuntimeId,
    source: Option<Box<RuntimeSourceReceipt>>,
    cause: JavaRuntimeLookupError,
    quarantine: Option<ManagedRuntimeQuarantineObligation>,
    _publication_lease: ManagedRuntimePublicationLease,
}

/// Separates failures before a filesystem effect from sealed post-effect evidence.
pub enum ManagedRuntimeRebuildError {
    Preparation(JavaRuntimeLookupError),
    Effect(Box<ManagedRuntimeFailureReceipt>),
}

impl std::fmt::Display for ManagedRuntimeRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preparation(error) => std::fmt::Display::fmt(error, formatter),
            Self::Effect(receipt) => std::fmt::Display::fmt(&receipt.cause, formatter),
        }
    }
}

impl std::fmt::Debug for ManagedRuntimeRebuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Preparation(_) => "ManagedRuntimeRebuildError::Preparation(..)",
            Self::Effect(_) => "ManagedRuntimeRebuildError::Effect(..)",
        })
    }
}

impl std::error::Error for ManagedRuntimeRebuildError {}

impl From<JavaRuntimeLookupError> for ManagedRuntimeRebuildError {
    fn from(error: JavaRuntimeLookupError) -> Self {
        Self::Preparation(error)
    }
}

impl ManagedRuntimeRebuildError {
    pub(crate) fn into_lookup_error(self) -> JavaRuntimeLookupError {
        match self {
            Self::Preparation(error) => error,
            Self::Effect(receipt) => receipt.cause,
        }
    }
}

impl ManagedRuntimeFailureReceipt {
    pub fn component(&self) -> &RuntimeId {
        &self.component
    }

    pub fn matches_cache(&self, cache: &ManagedRuntimeCache) -> bool {
        self.cache.shares_identity_with(cache)
    }

    pub fn quarantine_obligation(&self) -> Option<&ManagedRuntimeQuarantineObligation> {
        self.quarantine.as_ref()
    }

    pub fn matches_known_good_inventory(&self, inventory: &KnownGoodInventory) -> bool {
        self.source.as_ref().is_some_and(|source| {
            runtime_source_matches_known_good_inventory(&self.component, source.as_ref(), inventory)
        })
    }
}

impl ManagedRuntimeCommitReceipt {
    pub fn component(&self) -> &RuntimeId {
        &self.component
    }

    pub fn matches_cache(&self, cache: &ManagedRuntimeCache) -> bool {
        self.cache.shares_identity_with(cache)
    }

    pub fn quarantine_obligation(&self) -> Option<&ManagedRuntimeQuarantineObligation> {
        self.quarantine.as_ref()
    }

    pub fn matches_known_good_inventory(&self, inventory: &KnownGoodInventory) -> bool {
        self.source.as_ref().is_some_and(|source| {
            runtime_source_matches_known_good_inventory(&self.component, source, inventory)
        })
    }

    pub fn replace_known_good_runtime_projection(
        &self,
        active: &KnownGoodInventory,
    ) -> Result<KnownGoodInventory, crate::known_good::KnownGoodInventoryError> {
        let source = self
            .source
            .as_ref()
            .ok_or(crate::known_good::KnownGoodInventoryError::RuntimeIdentityMismatch)?;
        let runtime_only = crate::known_good::runtime_inventory_from_source(source)?;
        crate::known_good::replace_runtime_projection(active, runtime_only, &self.component)
    }

    pub async fn revalidate(
        &self,
        cache: &ManagedRuntimeCache,
        expected_component: &RuntimeId,
    ) -> bool {
        if !self.matches_cache(cache)
            || &self.component != expected_component
            || self
                .source
                .as_ref()
                .is_none_or(|source| source.component() != expected_component)
        {
            return false;
        }
        let Some(install_root) = cache.component_root(expected_component.as_str()) else {
            return false;
        };
        let Some(source) = self.source.as_ref() else {
            return false;
        };
        let Ok(canonical) = cache
            .authority()
            .and_then(|root| root.open_child(expected_component.as_str()))
        else {
            return false;
        };
        runtime_tree_matches_source(
            &canonical,
            &install_root,
            source,
            RuntimeTreeVerificationReason::ReceiptRevalidation,
        )
        .await
            && self
                .quarantine
                .as_ref()
                .is_none_or(|obligation| obligation.path_observation().is_present())
    }

    pub(super) fn into_verified_runtime(
        self,
        cache: &ManagedRuntimeCache,
        expected_component: &RuntimeId,
        required_major: i32,
    ) -> Result<VerifiedManagedRuntime, ManagedRuntimeRebuildError> {
        if !self.matches_cache(cache)
            || &self.component != expected_component
            || self
                .source
                .as_ref()
                .is_none_or(|source| source.component() != expected_component)
        {
            return Err(self.into_failure(JavaRuntimeLookupError::Install(
                "managed runtime commit cannot settle outside its verified authority".to_string(),
            )));
        }
        let runtime = match super::discovery::resolve_component_runtime(
            cache,
            expected_component,
            required_major,
        ) {
            Ok(runtime) => runtime,
            Err(error) => return Err(self.into_failure(error)),
        };
        let Self {
            source,
            _publication_lease,
            ..
        } = self;
        Ok(VerifiedManagedRuntime {
            runtime,
            source: source.expect("managed runtime commit receipt always retains its source"),
            _publication_lease,
        })
    }

    pub(crate) fn into_failure(self, cause: JavaRuntimeLookupError) -> ManagedRuntimeRebuildError {
        ManagedRuntimeRebuildError::Effect(Box::new(ManagedRuntimeFailureReceipt {
            cache: self.cache,
            component: self.component,
            source: self.source.map(Box::new),
            cause,
            quarantine: self.quarantine,
            _publication_lease: self._publication_lease,
        }))
    }
}

impl VerifiedManagedRuntime {
    pub(super) fn into_parts(self) -> (RuntimeRecord, RuntimeSourceReceipt) {
        let Self {
            runtime,
            source,
            _publication_lease,
        } = self;
        drop(_publication_lease);
        (runtime, source)
    }
}

impl ManagedRuntimeQuarantineObligation {
    pub fn component(&self) -> &RuntimeId {
        &self.component
    }

    pub fn matches_cache(&self, cache: &ManagedRuntimeCache) -> bool {
        self.cache.shares_identity_with(cache)
    }

    pub fn observation(&self) -> ManagedRuntimeQuarantineObservation {
        self.path_observation().into()
    }

    fn path_observation(&self) -> RuntimePathObservation {
        observe_runtime_child(
            &self.cache,
            &runtime_sidecar_name(self.component.as_str(), "quarantine"),
        )
    }
}

struct OwnedRuntimeStage {
    parent: ManagedDir,
    name: String,
    root: Option<ManagedDir>,
    projection: PathBuf,
}

impl OwnedRuntimeStage {
    fn new(parent: ManagedDir, name: String, root: ManagedDir, projection: PathBuf) -> Self {
        Self {
            parent,
            name,
            root: Some(root),
            projection,
        }
    }

    fn root(&self) -> &ManagedDir {
        self.root
            .as_ref()
            .expect("owned runtime stage is present before promotion")
    }

    fn projection(&self) -> &Path {
        &self.projection
    }

    fn take_root(&mut self) -> ManagedDir {
        self.root
            .take()
            .expect("owned runtime stage is present before promotion")
    }

    fn restore_root(&mut self, root: ManagedDir) {
        assert!(self.root.is_none(), "runtime stage root must be displaced");
        self.root = Some(root);
    }

    fn relinquish(&mut self) {
        self.root = None;
    }

    async fn cleanup(&mut self) -> std::io::Result<()> {
        let Some(root) = self.root.take() else {
            return Ok(());
        };
        self.parent
            .remove_child_tree(&self.name, root)
            .map_err(runtime_loader_io)
    }
}

async fn acquire_managed_runtime_publication_lease_until_cancelled(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    cancellation: &mut RuntimeCancellation,
) -> Result<Option<ManagedRuntimePublicationLease>, JavaRuntimeLookupError> {
    let component_lock = match cancellation
        .wait(cache.install_lock(component.as_str()).lock_owned())
        .await
    {
        Some(lock) => lock,
        None => return Ok(None),
    };
    Ok(Some(ManagedRuntimePublicationLease {
        _component_lock: component_lock,
    }))
}

pub(super) async fn verify_cached_managed_runtime_until_cancelled(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    required_major: i32,
    source: RuntimeSourceReceipt,
    cancellation: &mut RuntimeCancellation,
) -> Result<CachedManagedRuntimeVerification, JavaRuntimeLookupError> {
    if source.component() != component {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source component mismatch".to_string(),
        ));
    }
    if super::discovery::resolve_component_runtime(cache, component, required_major).is_err() {
        return Ok(CachedManagedRuntimeVerification::Mismatched(source));
    }
    let install_root = cache.component_root(component.as_str()).ok_or_else(|| {
        JavaRuntimeLookupError::Install(
            "runtime component is outside the managed cache vocabulary".to_string(),
        )
    })?;
    let Some(publication_lease) =
        acquire_managed_runtime_publication_lease_until_cancelled(cache, component, cancellation)
            .await?
    else {
        return Ok(CachedManagedRuntimeVerification::Cancelled);
    };
    let Ok(runtime) = super::discovery::resolve_component_runtime(cache, component, required_major)
    else {
        return Ok(CachedManagedRuntimeVerification::Mismatched(source));
    };
    let canonical = cache
        .authority()
        .and_then(|root| root.open_child(component.as_str()))
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let matches_source = runtime_tree_matches_source_until_cancelled(
        &canonical,
        &install_root,
        &source,
        RuntimeTreeVerificationReason::CachedSourceMatch,
        cancellation,
    )
    .await;
    if cancellation.is_cancelled() {
        return Ok(CachedManagedRuntimeVerification::Cancelled);
    }
    if !matches_source {
        return Ok(CachedManagedRuntimeVerification::Mismatched(source));
    }
    Ok(CachedManagedRuntimeVerification::Matched(
        VerifiedManagedRuntime {
            runtime,
            source,
            _publication_lease: publication_lease,
        },
    ))
}

pub(crate) async fn stage_managed_runtime(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source: RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<StagedManagedRuntime, JavaRuntimeLookupError> {
    let (_cancellation_tx, mut cancellation) = runtime_cancellation_channel();
    stage_managed_runtime_until_cancelled(cache, component, source, observer, &mut cancellation)
        .await?
        .ok_or_else(|| {
            JavaRuntimeLookupError::Install(
                "managed runtime staging stopped without a cancellation request".to_string(),
            )
        })
}

pub(super) async fn stage_managed_runtime_until_cancelled(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source: RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    cancellation: &mut RuntimeCancellation,
) -> Result<Option<StagedManagedRuntime>, JavaRuntimeLookupError> {
    if source.component() != component {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source component mismatch".to_string(),
        ));
    }
    let download_concurrency = runtime_file_download_concurrency();
    let admission = validate_managed_runtime_source(&source, download_concurrency)?;
    let install_root = cache.component_root(component.as_str()).ok_or_else(|| {
        JavaRuntimeLookupError::Install(
            "runtime component is outside the managed cache vocabulary".to_string(),
        )
    })?;
    let Some(publication_lease) =
        acquire_managed_runtime_publication_lease_until_cancelled(cache, component, cancellation)
            .await?
    else {
        return Ok(None);
    };
    let cache_root = cache
        .authority()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let staging_name = runtime_sidecar_name(component.as_str(), "staging");
    let staging_root = runtime_sidecar_path(&install_root, "staging");
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    remove_runtime_child_if_present(&cache_root, &staging_name)?;
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    let staging_directory = cache_root
        .create_child_new(&staging_name)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut stage =
        OwnedRuntimeStage::new(cache_root, staging_name, staging_directory, staging_root);
    let stage_result = materialize_runtime_tree_with_cancellation(
        component,
        stage.root(),
        stage.projection(),
        &source,
        observer,
        download_concurrency,
        admission.download_bytes,
        cancellation,
    )
    .await;
    if cancellation.is_cancelled() {
        stage
            .cleanup()
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        return Ok(None);
    }
    if let Err(error) = stage_result {
        let _ = stage.cleanup().await;
        return Err(error);
    }
    let tree_matches = runtime_tree_matches_source_until_cancelled(
        stage.root(),
        stage.projection(),
        &source,
        RuntimeTreeVerificationReason::StagePostMaterialization,
        cancellation,
    )
    .await;
    if cancellation.is_cancelled() {
        stage
            .cleanup()
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        return Ok(None);
    }
    if !tree_matches {
        let _ = stage.cleanup().await;
        return Err(JavaRuntimeLookupError::Install(
            "staged runtime does not match its authenticated source".to_string(),
        ));
    }
    Ok(Some(StagedManagedRuntime {
        cache: cache.clone(),
        component: component.clone(),
        install_root,
        stage,
        source: Some(source),
        publication_lease: Some(publication_lease),
    }))
}

pub(crate) async fn discard_staged_managed_runtime(
    mut staged: StagedManagedRuntime,
) -> Result<(), JavaRuntimeLookupError> {
    staged
        .stage
        .cleanup()
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

pub(crate) async fn publish_staged_managed_runtime(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Retain,
        PublishFailureMode::None,
        false,
    )
    .await
}

pub(super) async fn publish_staged_managed_runtime_and_finalize(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Finalize,
        PublishFailureMode::None,
        false,
    )
    .await
}

#[cfg(test)]
pub(super) async fn publish_staged_managed_runtime_with_promotion_failure_for_test(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Retain,
        PublishFailureMode::Promotion,
        false,
    )
    .await
}

#[cfg(test)]
pub(super) async fn publish_staged_managed_runtime_with_restoration_failure_for_test(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Retain,
        PublishFailureMode::Promotion,
        true,
    )
    .await
}

#[cfg(test)]
pub(super) async fn publish_staged_managed_runtime_with_finalization_failure_for_test(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Finalize,
        PublishFailureMode::Finalization,
        false,
    )
    .await
}

#[cfg(test)]
pub(super) async fn publish_staged_managed_runtime_with_rotation_failure_for_test(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Retain,
        PublishFailureMode::Rotation,
        false,
    )
    .await
}

#[cfg(test)]
pub(super) async fn publish_staged_managed_runtime_with_displacement_failure_for_test(
    staged: StagedManagedRuntime,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    publish_staged_managed_runtime_inner(
        staged,
        ManagedRuntimeQuarantineDisposition::Retain,
        PublishFailureMode::Displacement,
        false,
    )
    .await
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ManagedRuntimeQuarantineDisposition {
    Retain,
    Finalize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PublishFailureMode {
    None,
    Promotion,
    Finalization,
    Rotation,
    Displacement,
}

async fn publish_staged_managed_runtime_inner(
    mut staged: StagedManagedRuntime,
    quarantine_disposition: ManagedRuntimeQuarantineDisposition,
    failure_mode: PublishFailureMode,
    inject_restoration_failure: bool,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeRebuildError> {
    let expected_root = staged
        .cache
        .component_root(staged.component.as_str())
        .ok_or_else(|| {
            JavaRuntimeLookupError::Install(
                "runtime component is outside the managed cache vocabulary".to_string(),
            )
        })?;
    let expected_stage = runtime_sidecar_path(&expected_root, "staging");
    if expected_root != staged.install_root || staged.stage.projection() != expected_stage {
        let _ = staged.stage.cleanup().await;
        return Err(JavaRuntimeLookupError::Install(
            "managed runtime stage does not match its cache authority".to_string(),
        )
        .into());
    }
    let source = staged
        .source
        .take()
        .expect("staged managed runtime retains its authenticated source");
    #[cfg(test)]
    wait_for_runtime_test_hook(RuntimeTestHookPoint::Publication, &staged.install_root).await;
    if source.component() != &staged.component
        || !runtime_tree_matches_source(
            staged.stage.root(),
            staged.stage.projection(),
            &source,
            RuntimeTreeVerificationReason::PublicationPrePromotion,
        )
        .await
    {
        let _ = staged.stage.cleanup().await;
        return Err(JavaRuntimeLookupError::Install(
            "staged runtime failed exact pre-promotion verification".to_string(),
        )
        .into());
    }

    let cache_root = staged
        .cache
        .authority()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let canonical_name = staged.component.as_str();
    let quarantine_name = runtime_sidecar_name(canonical_name, "quarantine");
    let mut canonical = cache_root
        .open_child_if_exists(canonical_name)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut quarantine = cache_root
        .open_child_if_exists(&quarantine_name)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut publication_effect_started = false;

    if let Some(retained) = canonical.as_ref()
        && runtime_tree_matches_source(
            retained,
            &staged.install_root,
            &source,
            RuntimeTreeVerificationReason::CanonicalReuse,
        )
        .await
    {
        staged
            .stage
            .cleanup()
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        return Ok(managed_runtime_commit_receipt(
            &mut staged,
            source,
            quarantine.is_some(),
        ));
    }

    if canonical.is_none()
        && let Some(retained) = quarantine.take()
    {
        match cache_root.move_child_guarded_no_replace(
            &quarantine_name,
            retained,
            &cache_root,
            canonical_name,
        ) {
            Ok(restored) => canonical = Some(restored),
            Err(failure) => {
                let _ = staged.stage.cleanup().await;
                return Err(classify_managed_runtime_publish_failure(
                    &mut staged,
                    failure == ManagedDirectoryMoveFailure::MoveAttempted,
                    source,
                    JavaRuntimeLookupError::Install(format!(
                        "managed runtime quarantine restoration failed: {failure:?}"
                    )),
                ));
            }
        }
        publication_effect_started = true;
    }

    if let Some(retained) = quarantine.take() {
        // Recursive removal can partially mutate the quarantine before reporting failure.
        publication_effect_started = true;
        let rotation_result = if failure_mode == PublishFailureMode::Rotation {
            Err(JavaRuntimeLookupError::Install(
                "injected managed runtime quarantine rotation failure".to_string(),
            ))
        } else {
            cache_root
                .remove_child_tree(&quarantine_name, retained)
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
        };
        if let Err(error) = rotation_result {
            let _ = staged.stage.cleanup().await;
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(format!(
                    "managed runtime quarantine rotation failed: {error}"
                )),
            ));
        }
    }

    let displaced_canonical = if let Some(retained) = canonical.take() {
        let displacement_result = if failure_mode == PublishFailureMode::Displacement {
            Err(ManagedDirectoryMoveFailure::BeforeMove)
        } else {
            cache_root
                .move_child_guarded_no_replace(
                    canonical_name,
                    retained,
                    &cache_root,
                    &quarantine_name,
                )
                .map(|moved| {
                    quarantine = Some(moved);
                })
        };
        if let Err(error) = displacement_result {
            let _ = staged.stage.cleanup().await;
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started || error == ManagedDirectoryMoveFailure::MoveAttempted,
                source,
                JavaRuntimeLookupError::Install(format!(
                    "managed runtime canonical displacement failed: {error:?}"
                )),
            ));
        }
        publication_effect_started = true;
        true
    } else {
        false
    };

    let promotion_result = if failure_mode == PublishFailureMode::Promotion {
        Err(ManagedDirectoryMoveFailure::BeforeMove)
    } else {
        let stage_root = staged.stage.take_root();
        cache_root.move_child_guarded_no_replace(
            &staged.stage.name,
            stage_root,
            &cache_root,
            canonical_name,
        )
    };
    let canonical = match promotion_result {
        Ok(canonical) => canonical,
        Err(promotion_error) => {
            if staged.stage.root.is_none()
                && let Ok(Some(restored_stage)) =
                    cache_root.open_child_if_exists(&staged.stage.name)
            {
                staged.stage.restore_root(restored_stage);
            }
            let restore_result = if displaced_canonical && inject_restoration_failure {
                Err(ManagedDirectoryMoveFailure::BeforeMove)
            } else if displaced_canonical {
                cache_root
                    .move_child_guarded_no_replace(
                        &quarantine_name,
                        quarantine
                            .take()
                            .expect("displaced runtime retains quarantine"),
                        &cache_root,
                        canonical_name,
                    )
                    .map(drop)
            } else {
                Ok(())
            };
            let _ = staged.stage.cleanup().await;
            if restore_result.is_err() {
                return Err(classify_managed_runtime_publish_failure(
                    &mut staged,
                    publication_effect_started,
                    source,
                    JavaRuntimeLookupError::Install(
                        "runtime promotion and canonical restoration both failed".to_string(),
                    ),
                ));
            }
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(format!("{promotion_error:?}")),
            ));
        }
    };
    publication_effect_started = true;

    if !runtime_tree_matches_source(
        &canonical,
        &staged.install_root,
        &source,
        RuntimeTreeVerificationReason::PublicationPostPromotion,
    )
    .await
    {
        let failed_tree_result = cache_root.move_child_guarded_no_replace(
            canonical_name,
            canonical,
            &cache_root,
            &staged.stage.name,
        );
        let Ok(isolated) = failed_tree_result else {
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(
                    "published runtime failed verification and could not be isolated".to_string(),
                ),
            ));
        };
        staged.stage.restore_root(isolated);
        let restore_result = if displaced_canonical {
            cache_root
                .move_child_guarded_no_replace(
                    &quarantine_name,
                    quarantine
                        .take()
                        .expect("displaced runtime retains quarantine"),
                    &cache_root,
                    canonical_name,
                )
                .map(drop)
        } else {
            Ok(())
        };
        let _ = staged.stage.cleanup().await;
        if restore_result.is_err() {
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(
                    "runtime postcondition and canonical restoration both failed".to_string(),
                ),
            ));
        }
        return Err(classify_managed_runtime_publish_failure(
            &mut staged,
            publication_effect_started,
            source,
            JavaRuntimeLookupError::Install(
                "published runtime failed exact postcondition verification".to_string(),
            ),
        ));
    }

    staged.stage.relinquish();
    if displaced_canonical
        && quarantine_disposition == ManagedRuntimeQuarantineDisposition::Finalize
        && let Err(error) = finalize_runtime_quarantine(
            &cache_root,
            &quarantine_name,
            quarantine.take(),
            failure_mode,
        )
    {
        return Err(classify_managed_runtime_publish_failure(
            &mut staged,
            publication_effect_started,
            source,
            JavaRuntimeLookupError::Install(format!(
                "managed runtime quarantine finalization failed: {error}"
            )),
        ));
    }
    Ok(managed_runtime_commit_receipt(
        &mut staged,
        source,
        displaced_canonical
            && quarantine_disposition == ManagedRuntimeQuarantineDisposition::Retain,
    ))
}

fn finalize_runtime_quarantine(
    cache_root: &ManagedDir,
    quarantine_name: &str,
    quarantine: Option<ManagedDir>,
    failure_mode: PublishFailureMode,
) -> Result<(), JavaRuntimeLookupError> {
    if failure_mode == PublishFailureMode::Finalization {
        return Err(JavaRuntimeLookupError::Install(
            "injected managed runtime quarantine finalization failure".to_string(),
        ));
    }
    if let Some(quarantine) = quarantine {
        cache_root
            .remove_child_tree(quarantine_name, quarantine)
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }
    Ok(())
}

fn classify_managed_runtime_publish_failure(
    staged: &mut StagedManagedRuntime,
    publication_effect_started: bool,
    source: RuntimeSourceReceipt,
    cause: JavaRuntimeLookupError,
) -> ManagedRuntimeRebuildError {
    if !publication_effect_started {
        return ManagedRuntimeRebuildError::Preparation(cause);
    }
    ManagedRuntimeRebuildError::Effect(Box::new(ManagedRuntimeFailureReceipt {
        cache: staged.cache.clone(),
        component: staged.component.clone(),
        source: Some(Box::new(source)),
        cause,
        quarantine: observe_runtime_child(
            &staged.cache,
            &runtime_sidecar_name(staged.component.as_str(), "quarantine"),
        )
        .retains_obligation()
        .then(|| ManagedRuntimeQuarantineObligation {
            cache: staged.cache.clone(),
            component: staged.component.clone(),
        }),
        _publication_lease: staged
            .publication_lease
            .take()
            .expect("managed runtime publication retains its lease until terminal settlement"),
    }))
}

fn managed_runtime_commit_receipt(
    staged: &mut StagedManagedRuntime,
    source: RuntimeSourceReceipt,
    quarantine_present: bool,
) -> ManagedRuntimeCommitReceipt {
    ManagedRuntimeCommitReceipt {
        cache: staged.cache.clone(),
        component: staged.component.clone(),
        source: Some(source),
        quarantine: quarantine_present.then(|| ManagedRuntimeQuarantineObligation {
            cache: staged.cache.clone(),
            component: staged.component.clone(),
        }),
        _publication_lease: staged
            .publication_lease
            .take()
            .expect("managed runtime publication retains its lease until terminal settlement"),
    }
}

#[cfg(test)]
static RUNTIME_TREE_VERIFICATION_COUNTS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, RuntimeTreeVerificationCounts>>,
> = std::sync::OnceLock::new();

fn record_runtime_tree_verification(
    root: &Path,
    source: &RuntimeSourceReceipt,
    reason: RuntimeTreeVerificationReason,
) {
    #[cfg(test)]
    {
        let scope_root = if matches!(
            reason,
            RuntimeTreeVerificationReason::StagePostMaterialization
                | RuntimeTreeVerificationReason::PublicationPrePromotion
        ) {
            root.parent()
                .map(|parent| parent.join(source.component().as_str()))
                .unwrap_or_else(|| root.to_path_buf())
        } else {
            root.to_path_buf()
        };
        let mut counts = RUNTIME_TREE_VERIFICATION_COUNTS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .expect("runtime tree verification counter registry");
        if let Some(counts) = counts.get_mut(&scope_root) {
            counts.record(reason);
        }
    }
    #[cfg(not(test))]
    let _ = (root, source, reason);
}

#[cfg(test)]
pub(crate) fn register_runtime_tree_verification_counts_for_test(root: &Path) {
    RUNTIME_TREE_VERIFICATION_COUNTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("runtime tree verification counter registry")
        .insert(root.to_path_buf(), RuntimeTreeVerificationCounts::default());
}

#[cfg(test)]
pub(crate) fn take_runtime_tree_verification_counts_for_test(
    root: &Path,
) -> RuntimeTreeVerificationCounts {
    RUNTIME_TREE_VERIFICATION_COUNTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .expect("runtime tree verification counter registry")
        .remove(root)
        .unwrap_or_default()
}

pub(super) async fn runtime_tree_matches_source(
    root: &ManagedDir,
    projection: &Path,
    source: &RuntimeSourceReceipt,
    reason: RuntimeTreeVerificationReason,
) -> bool {
    let (_cancellation_sender, cancellation) = runtime_cancellation_channel();
    runtime_tree_matches_source_inner(
        root,
        projection,
        source,
        reason,
        cancellation.thread_cancellation(),
    )
    .await
}

pub(super) async fn runtime_tree_matches_source_until_cancelled(
    root: &ManagedDir,
    projection: &Path,
    source: &RuntimeSourceReceipt,
    reason: RuntimeTreeVerificationReason,
    cancellation: &RuntimeCancellation,
) -> bool {
    runtime_tree_matches_source_inner(
        root,
        projection,
        source,
        reason,
        cancellation.thread_cancellation(),
    )
    .await
}

async fn runtime_tree_matches_source_inner(
    root: &ManagedDir,
    projection: &Path,
    source: &RuntimeSourceReceipt,
    reason: RuntimeTreeVerificationReason,
    cancellation: RuntimeThreadCancellation,
) -> bool {
    record_runtime_tree_verification(projection, source, reason);
    if cancellation.is_cancelled() {
        return false;
    }
    let root = root.clone();
    let component = source.component().clone();
    let source_manifest = source.manifest().clone();
    let worker_cancellation = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        if worker_cancellation.is_cancelled() {
            return false;
        }
        managed_runtime_tree_matches_manifest(
            &component,
            &root,
            &source_manifest,
            &worker_cancellation,
        )
    })
    .await
    .unwrap_or(false)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RuntimeTreeNodeKind {
    Directory,
    File,
    Link,
}

pub(super) fn managed_runtime_tree_matches_manifest(
    component: &RuntimeId,
    root: &ManagedDir,
    manifest: &ComponentManifest,
    cancellation: &RuntimeThreadCancellation,
) -> bool {
    managed_runtime_tree_matches_manifest_inner(component, root, manifest, cancellation, true)
}

pub(super) fn managed_runtime_tree_matches_manifest_without_ready_marker(
    component: &RuntimeId,
    root: &ManagedDir,
    manifest: &ComponentManifest,
    cancellation: &RuntimeThreadCancellation,
) -> bool {
    managed_runtime_tree_matches_manifest_inner(component, root, manifest, cancellation, false)
}

fn managed_runtime_tree_matches_manifest_inner(
    component: &RuntimeId,
    root: &ManagedDir,
    manifest: &ComponentManifest,
    cancellation: &RuntimeThreadCancellation,
    ready_marker_present: bool,
) -> bool {
    let Ok(expected_proof) = component_manifest_proof_bytes(manifest) else {
        return false;
    };
    if !matches!(
        root.read_authenticated(
            COMPONENT_MANIFEST_PROOF_FILE,
            Some(expected_proof.len() as u64),
            None,
        ),
        Ok(actual) if actual == expected_proof
    ) {
        return false;
    }
    if ready_marker_present {
        if !matches!(
            root.read_authenticated(".axial-ready", Some(5), None),
            Ok(actual) if actual == b"ready"
        ) {
            return false;
        }
    } else if !matches!(root.exact_entry_kind(".axial-ready"), Ok(None)) {
        return false;
    }

    let mut expected = HashMap::new();
    if !insert_runtime_tree_node(
        &mut expected,
        PathBuf::from(COMPONENT_MANIFEST_PROOF_FILE),
        RuntimeTreeNodeKind::File,
    ) {
        return false;
    }
    if ready_marker_present
        && !insert_runtime_tree_node(
            &mut expected,
            PathBuf::from(".axial-ready"),
            RuntimeTreeNodeKind::File,
        )
    {
        return false;
    }
    for (relative, file) in &manifest.files {
        let Ok(path) = component_manifest_destination(component, Path::new(""), relative) else {
            return false;
        };
        let kind = match file.kind.as_str() {
            "directory" => RuntimeTreeNodeKind::Directory,
            "file" => RuntimeTreeNodeKind::File,
            "link" => RuntimeTreeNodeKind::Link,
            _ => return false,
        };
        if !insert_runtime_tree_node(&mut expected, path, kind) {
            return false;
        }
    }

    let expected_node_count = expected.len();
    let mut observed_node_count = 0_usize;
    let mut directories = vec![(root.clone(), PathBuf::new())];
    while let Some((directory, prefix)) = directories.pop() {
        if cancellation.is_cancelled() {
            return false;
        }
        let Ok(entries) = directory.guarded_entries_bounded(MAX_RUNTIME_TREE_ENTRIES) else {
            return false;
        };
        for entry in entries {
            let Some(name) = entry.utf8_name() else {
                return false;
            };
            if PortableFileName::new_exact(name).is_err() {
                return false;
            }
            observed_node_count = observed_node_count.saturating_add(1);
            if observed_node_count > expected_node_count {
                return false;
            }
            let relative = prefix.join(name);
            let actual = match entry.kind() {
                axial_fs::EntryKind::Directory => RuntimeTreeNodeKind::Directory,
                axial_fs::EntryKind::File => RuntimeTreeNodeKind::File,
                axial_fs::EntryKind::Link => RuntimeTreeNodeKind::Link,
                axial_fs::EntryKind::Other => return false,
            };
            if expected.remove(&relative) != Some(actual) {
                return false;
            }
            if actual == RuntimeTreeNodeKind::Directory {
                let Ok(child) = directory.open_observed_child(&entry) else {
                    return false;
                };
                directories.push((child, relative));
            }
        }
    }
    if !expected.is_empty() {
        return false;
    }

    for (relative, file) in &manifest.files {
        if cancellation.is_cancelled() {
            return false;
        }
        let Ok(relative_path) = PortableRelativePath::new_exact(relative) else {
            return false;
        };
        match file.kind.as_str() {
            "directory" => {}
            "file" => {
                let Some(raw) = file
                    .downloads
                    .as_ref()
                    .and_then(|downloads| downloads.raw.as_ref())
                else {
                    return false;
                };
                let (Some(size), Some(sha1)) = (raw.size, raw.sha1.as_deref()) else {
                    return false;
                };
                let Some(sha1) = runtime_sha1_bytes(sha1) else {
                    return false;
                };
                if root
                    .verify_relative_file_sha1(&relative_path, size, &sha1, || {
                        if cancellation.is_cancelled() {
                            Err(crate::loaders::types::LoaderError::Verify(
                                "runtime verification was cancelled".to_string(),
                            ))
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
                {
                    return false;
                }
                #[cfg(unix)]
                if file.executable
                    && !matches!(root.relative_file_is_executable(&relative_path), Ok(true))
                {
                    return false;
                }
            }
            "link" => {
                #[cfg(unix)]
                {
                    let Some(target) = file.target.as_deref() else {
                        return false;
                    };
                    if !matches!(
                        root.read_symlink_relative(&relative_path),
                        Ok(actual) if actual == std::ffi::OsStr::new(target)
                    ) {
                        return false;
                    }
                }
                #[cfg(not(unix))]
                return false;
            }
            _ => return false,
        }
    }
    root.revalidate().is_ok() && !cancellation.is_cancelled()
}

#[cfg(test)]
fn runtime_tree_shape_matches_manifest(
    component: &RuntimeId,
    root: &Path,
    manifest: &ComponentManifest,
) -> bool {
    runtime_tree_shape_matches_manifest_inner(component, root, manifest, None)
}

#[cfg(test)]
fn runtime_tree_shape_matches_manifest_inner(
    component: &RuntimeId,
    root: &Path,
    manifest: &ComponentManifest,
    cancellation: Option<&RuntimeThreadCancellation>,
) -> bool {
    if cancellation.is_some_and(RuntimeThreadCancellation::is_cancelled) {
        return false;
    }
    let filesystem_root = runtime_filesystem_path(root).into_owned();
    let Ok(root_metadata) = std::fs::symlink_metadata(&filesystem_root) else {
        return false;
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return false;
    }

    let mut expected = HashMap::new();
    if !insert_runtime_tree_node(
        &mut expected,
        PathBuf::from(COMPONENT_MANIFEST_PROOF_FILE),
        RuntimeTreeNodeKind::File,
    ) || !insert_runtime_tree_node(
        &mut expected,
        PathBuf::from(".axial-ready"),
        RuntimeTreeNodeKind::File,
    ) {
        return false;
    }
    for (relative, file) in &manifest.files {
        if cancellation.is_some_and(RuntimeThreadCancellation::is_cancelled) {
            return false;
        }
        let Ok(path) = component_manifest_destination(component, Path::new(""), relative) else {
            return false;
        };
        let kind = match file.kind.as_str() {
            "directory" => RuntimeTreeNodeKind::Directory,
            "file" => RuntimeTreeNodeKind::File,
            "link" => RuntimeTreeNodeKind::Link,
            _ => return false,
        };
        if !insert_runtime_tree_node(&mut expected, path, kind) {
            return false;
        }
    }

    let expected_node_count = expected.len();
    let mut observed_node_count = 0_usize;
    let mut directories = vec![filesystem_root.clone()];
    while let Some(directory) = directories.pop() {
        if cancellation.is_some_and(RuntimeThreadCancellation::is_cancelled) {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            if cancellation.is_some_and(RuntimeThreadCancellation::is_cancelled) {
                return false;
            }
            observed_node_count = observed_node_count.saturating_add(1);
            if observed_node_count > expected_node_count {
                return false;
            }
            let Ok(entry) = entry else {
                return false;
            };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&filesystem_root) else {
                return false;
            };
            let Ok(metadata) = std::fs::symlink_metadata(runtime_filesystem_path(&path).as_ref())
            else {
                return false;
            };
            let actual = if metadata.file_type().is_symlink() {
                RuntimeTreeNodeKind::Link
            } else if metadata.is_dir() {
                RuntimeTreeNodeKind::Directory
            } else if metadata.is_file() {
                RuntimeTreeNodeKind::File
            } else {
                return false;
            };
            if expected.remove(relative) != Some(actual) {
                return false;
            }
            if actual == RuntimeTreeNodeKind::Directory {
                directories.push(path);
            }
        }
    }
    expected.is_empty()
}

#[cfg(test)]
mod runtime_tree_shape_tests {
    use super::{ComponentManifest, ComponentManifestFile, runtime_tree_shape_matches_manifest};
    use std::collections::HashMap;

    #[test]
    fn accepts_entries_returned_from_the_platform_filesystem_root() {
        let root = tempfile::tempdir().expect("runtime tree root");
        std::fs::create_dir(root.path().join("bin")).expect("runtime bin directory");
        std::fs::write(root.path().join("bin/java.exe"), b"java").expect("runtime executable");
        std::fs::write(
            root.path().join(".axial-runtime-manifest.json"),
            b"manifest",
        )
        .expect("runtime manifest proof");
        std::fs::write(root.path().join(".axial-ready"), b"ready").expect("runtime ready marker");
        let manifest = ComponentManifest {
            files: HashMap::from([
                (
                    "bin".to_string(),
                    ComponentManifestFile {
                        kind: "directory".to_string(),
                        executable: false,
                        downloads: None,
                        target: None,
                    },
                ),
                (
                    "bin/java.exe".to_string(),
                    ComponentManifestFile {
                        kind: "file".to_string(),
                        executable: true,
                        downloads: None,
                        target: None,
                    },
                ),
            ]),
        };

        assert!(runtime_tree_shape_matches_manifest(
            &super::RuntimeId::from("java-runtime-gamma"),
            root.path(),
            &manifest,
        ));
    }
}

fn insert_runtime_tree_node(
    expected: &mut HashMap<PathBuf, RuntimeTreeNodeKind>,
    path: PathBuf,
    kind: RuntimeTreeNodeKind,
) -> bool {
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        if candidate.as_os_str().is_empty() {
            break;
        }
        if expected
            .insert(candidate.to_path_buf(), RuntimeTreeNodeKind::Directory)
            .is_some_and(|existing| existing != RuntimeTreeNodeKind::Directory)
        {
            return false;
        }
        parent = candidate.parent();
    }
    expected
        .insert(path, kind)
        .is_none_or(|existing| existing == kind)
}

fn record_runtime_manifest_prefix_spellings(
    prefixes: &mut HashMap<PortablePathKey, String>,
    canonical_path: &Path,
    filesystem_key: &PortablePathKey,
) -> bool {
    let Some(path_prefixes) = runtime_manifest_path_prefixes(canonical_path, filesystem_key) else {
        return false;
    };
    for (folded_prefix, canonical_prefix) in path_prefixes {
        if prefixes
            .get(&folded_prefix)
            .is_some_and(|existing| existing != &canonical_prefix)
        {
            return false;
        }
        prefixes.insert(folded_prefix, canonical_prefix);
    }
    true
}

fn runtime_manifest_prefix_spellings_match(
    prefixes: &HashMap<PortablePathKey, String>,
    canonical_path: &Path,
    filesystem_key: &PortablePathKey,
) -> bool {
    runtime_manifest_path_prefixes(canonical_path, filesystem_key).is_some_and(|path_prefixes| {
        path_prefixes
            .into_iter()
            .all(|(folded, canonical)| prefixes.get(&folded) == Some(&canonical))
    })
}

fn runtime_manifest_path_prefixes(
    canonical_path: &Path,
    filesystem_key: &PortablePathKey,
) -> Option<Vec<(PortablePathKey, String)>> {
    let canonical_segments = canonical_path
        .iter()
        .map(|segment| segment.to_str())
        .collect::<Option<Vec<_>>>();
    let canonical_segments = canonical_segments?;
    let folded_segments = filesystem_key.as_str().split('/').collect::<Vec<_>>();
    if canonical_segments.len() != folded_segments.len() {
        return None;
    }

    let mut prefixes = Vec::with_capacity(canonical_segments.len());
    let mut canonical_prefix = String::new();
    let mut folded_prefix = String::new();
    for (canonical_segment, folded_segment) in canonical_segments.into_iter().zip(folded_segments) {
        if !canonical_prefix.is_empty() {
            canonical_prefix.push('/');
            folded_prefix.push('/');
        }
        canonical_prefix.push_str(canonical_segment);
        folded_prefix.push_str(folded_segment);
        prefixes.push((
            PortableRelativePath::new(&folded_prefix).ok()?.key(),
            canonical_prefix.clone(),
        ));
    }
    Some(prefixes)
}

enum KnownGoodRuntimeExpectation {
    ExactBytes {
        kind: KnownGoodArtifactKind,
        digest: String,
        size: u64,
    },
    File {
        kind: KnownGoodArtifactKind,
        digest: String,
        size: u64,
    },
    Directory,
    Link {
        kind: KnownGoodArtifactKind,
        target: String,
    },
}

fn runtime_source_matches_known_good_inventory(
    component: &RuntimeId,
    source: &RuntimeSourceReceipt,
    inventory: &KnownGoodInventory,
) -> bool {
    if source.component() != component {
        return false;
    }
    let Ok(proof) = component_manifest_proof_bytes(source.manifest()) else {
        return false;
    };
    let mut expected = HashMap::new();
    if expected
        .insert(
            COMPONENT_MANIFEST_PROOF_FILE.to_string(),
            exact_known_good_expectation(KnownGoodArtifactKind::RuntimeManifestProof, &proof),
        )
        .is_some()
        || expected
            .insert(
                ".axial-ready".to_string(),
                exact_known_good_expectation(KnownGoodArtifactKind::RuntimeReadyMarker, b"ready"),
            )
            .is_some()
    {
        return false;
    }

    let plan = plan_runtime_manifest_files(source.manifest().files.clone());
    if !plan.other_entries.is_empty() || plan.file_entries.is_empty() {
        return false;
    }
    for (path, _) in plan.directory_entries {
        if expected
            .insert(path, KnownGoodRuntimeExpectation::Directory)
            .is_some()
        {
            return false;
        }
    }
    let java_path = super::layout::runtime_java_relative_path();
    let mut saw_java = false;
    for (path, file) in plan.file_entries {
        let Some(raw) = file.downloads.and_then(|downloads| downloads.raw) else {
            return false;
        };
        let (Some(size), Some(digest)) = (raw.size, raw.sha1) else {
            return false;
        };
        if !runtime_sha1_is_valid(&digest) {
            return false;
        }
        let kind = if path == java_path {
            saw_java = true;
            KnownGoodArtifactKind::RuntimeExecutable
        } else {
            KnownGoodArtifactKind::RuntimeFile
        };
        if expected
            .insert(
                path,
                KnownGoodRuntimeExpectation::File {
                    kind,
                    digest: digest.to_ascii_lowercase(),
                    size,
                },
            )
            .is_some()
        {
            return false;
        }
    }
    for (path, file) in plan.link_entries {
        let Some(target) = file.target else {
            return false;
        };
        let kind = if path == java_path {
            saw_java = true;
            KnownGoodArtifactKind::RuntimeExecutable
        } else {
            KnownGoodArtifactKind::RuntimeLink
        };
        if expected
            .insert(path, KnownGoodRuntimeExpectation::Link { kind, target })
            .is_some()
        {
            return false;
        }
    }
    if !saw_java {
        return false;
    }

    for entry in inventory.entries() {
        let KnownGoodRoot::ManagedRuntime {
            component: inventory_component,
        } = entry.root()
        else {
            continue;
        };
        if inventory_component.as_str() != component.as_str() {
            return false;
        }
        let Some(expectation) = expected.remove(entry.path().as_str()) else {
            return false;
        };
        if !known_good_runtime_entry_matches(entry, &expectation) {
            return false;
        }
    }
    expected.is_empty()
}

fn exact_known_good_expectation(
    kind: KnownGoodArtifactKind,
    bytes: &[u8],
) -> KnownGoodRuntimeExpectation {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    KnownGoodRuntimeExpectation::ExactBytes {
        kind,
        digest: format!("{:x}", hasher.finalize()),
        size: bytes.len() as u64,
    }
}

fn known_good_runtime_entry_matches(
    entry: &crate::known_good::KnownGoodEntry,
    expected: &KnownGoodRuntimeExpectation,
) -> bool {
    match (expected, entry.integrity()) {
        (
            KnownGoodRuntimeExpectation::ExactBytes { kind, digest, size },
            KnownGoodIntegrity::ExactBytes {
                digest: actual_digest,
                size: actual_size,
            },
        ) => entry.kind() == *kind && actual_digest.as_str() == digest && actual_size == size,
        (
            KnownGoodRuntimeExpectation::File { kind, digest, size },
            KnownGoodIntegrity::Sha1 {
                digest: actual_digest,
                size: actual_size,
            },
        ) => entry.kind() == *kind && actual_digest.as_str() == digest && actual_size == size,
        (KnownGoodRuntimeExpectation::Directory, KnownGoodIntegrity::Directory) => {
            entry.kind() == KnownGoodArtifactKind::RuntimeDirectory
        }
        (KnownGoodRuntimeExpectation::Link { kind, target }, KnownGoodIntegrity::LinkTarget(_)) => {
            entry.kind() == *kind && known_good_link_target_matches(entry, Path::new(target))
        }
        _ => false,
    }
}

fn runtime_sidecar_path(install_root: &Path, suffix: &str) -> PathBuf {
    let mut name = install_root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("runtime"))
        .to_os_string();
    name.push(".");
    name.push(suffix);
    install_root.with_file_name(name)
}

fn runtime_sidecar_name(component: &str, suffix: &str) -> String {
    format!("{component}.{suffix}")
}

fn remove_runtime_child_if_present(
    root: &ManagedDir,
    name: &str,
) -> Result<(), JavaRuntimeLookupError> {
    if let Some(child) = root
        .open_child_if_exists(name)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
    {
        root.remove_child_tree(name, child)
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }
    Ok(())
}

fn observe_runtime_child(cache: &ManagedRuntimeCache, name: &str) -> RuntimePathObservation {
    match cache
        .authority()
        .and_then(|root| root.open_child_if_exists(name))
    {
        Ok(Some(_)) => RuntimePathObservation::Present,
        Ok(None) => RuntimePathObservation::Absent,
        Err(_) => RuntimePathObservation::Indeterminate,
    }
}

fn runtime_loader_io(error: crate::loaders::types::LoaderError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimePathObservation {
    Present,
    Absent,
    Indeterminate,
}

impl RuntimePathObservation {
    fn is_present(self) -> bool {
        self == Self::Present
    }

    fn retains_obligation(self) -> bool {
        self != Self::Absent
    }
}

impl From<RuntimePathObservation> for ManagedRuntimeQuarantineObservation {
    fn from(observation: RuntimePathObservation) -> Self {
        match observation {
            RuntimePathObservation::Present => Self::Present,
            RuntimePathObservation::Absent => Self::Absent,
            RuntimePathObservation::Indeterminate => Self::Indeterminate,
        }
    }
}

#[cfg(test)]
pub(crate) fn block_runtime_publication_for_test(install_root: &Path) -> RuntimeTestGate {
    arm_runtime_test_hook(RuntimeTestHookPoint::Publication, install_root)
}

#[cfg(test)]
pub(crate) fn runtime_publication_lock_available_for_test(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
) -> bool {
    cache
        .install_lock(component.as_str())
        .try_lock_owned()
        .is_ok()
}

#[cfg(test)]
impl StagedManagedRuntime {
    pub(super) fn staging_root_for_test(&self) -> &Path {
        self.stage.projection()
    }
}

#[cfg(test)]
impl ManagedRuntimeCommitReceipt {
    pub(super) fn quarantine_root_for_test(&self) -> Option<PathBuf> {
        self.quarantine.as_ref().and_then(|quarantine| {
            quarantine
                .cache
                .component_root(quarantine.component.as_str())
                .map(|root| runtime_sidecar_path(&root, "quarantine"))
        })
    }
}

pub(super) async fn install_ephemeral_processor_runtime(
    component: &RuntimeId,
    dest_dir: &ManagedDir,
    source: &RuntimeSourceReceipt,
    max_entries: usize,
    max_bytes: u64,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    let admission = validate_ephemeral_processor_manifest(source, max_entries, max_bytes)?;
    let projection = dest_dir.path();
    dest_dir
        .validate_absolute_projection(&projection)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let materialized = materialize_runtime_tree_with_concurrency(
        component,
        dest_dir,
        &projection,
        source,
        observer,
        1,
        admission.download_bytes,
    )
    .await;
    dest_dir
        .validate_absolute_projection(&projection)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    materialized
}

async fn materialize_runtime_tree_with_concurrency(
    component: &RuntimeId,
    dest_dir: &ManagedDir,
    projection: &Path,
    source: &RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
    admitted_download_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    let (_cancellation_sender, mut cancellation) = runtime_cancellation_channel();
    materialize_runtime_tree_with_cancellation(
        component,
        dest_dir,
        projection,
        source,
        observer,
        download_concurrency,
        admitted_download_bytes,
        &mut cancellation,
    )
    .await
}

async fn materialize_runtime_tree_with_cancellation(
    component: &RuntimeId,
    dest_dir: &ManagedDir,
    projection: &Path,
    source: &RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
    admitted_download_bytes: u64,
    cancellation: &mut RuntimeCancellation,
) -> Result<(), JavaRuntimeLookupError> {
    if source.component() != component {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source component mismatch".to_string(),
        ));
    }
    dest_dir
        .revalidate()
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }
    let install_result = async {
        let component_manifest = source.manifest();
        persist_component_manifest_proof(dest_dir, component_manifest)?;

        install_runtime_manifest_files_with_concurrency(
            component,
            dest_dir,
            projection,
            component_manifest.files.clone(),
            observer,
            download_concurrency,
            admitted_download_bytes,
            cancellation,
        )
        .await?;

        if cancellation.is_cancelled() {
            return Err(runtime_materialization_cancelled());
        }

        dest_dir
            .revalidate()
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        if !managed_runtime_executable_ready(dest_dir) {
            return Err(JavaRuntimeLookupError::Install(format!(
                "installed runtime {} is incomplete",
                component.as_str()
            )));
        }

        Ok(())
    }
    .await;

    install_result?;

    dest_dir
        .write_new_exact(".axial-ready", b"ready")
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }

    Ok(())
}

fn runtime_materialization_cancelled() -> JavaRuntimeLookupError {
    JavaRuntimeLookupError::Install("runtime staging was cancelled".to_string())
}

fn validate_ephemeral_processor_manifest(
    source: &RuntimeSourceReceipt,
    max_entries: usize,
    max_bytes: u64,
) -> Result<RuntimeManifestAdmission, JavaRuntimeLookupError> {
    validate_runtime_manifest_contract(
        source.component(),
        source.manifest(),
        source.bytes().len() as u64,
        max_entries.min(MAX_RUNTIME_TREE_ENTRIES),
        max_bytes.min(MAX_RUNTIME_TREE_TOTAL_BYTES),
        1,
    )
}

fn validate_managed_runtime_source(
    source: &RuntimeSourceReceipt,
    download_concurrency: usize,
) -> Result<RuntimeManifestAdmission, JavaRuntimeLookupError> {
    validate_runtime_manifest_contract(
        source.component(),
        source.manifest(),
        source.bytes().len() as u64,
        MAX_RUNTIME_TREE_ENTRIES,
        MAX_RUNTIME_TREE_TOTAL_BYTES,
        download_concurrency,
    )
}

pub(super) fn persisted_runtime_manifest_contract_is_valid(
    component: &RuntimeId,
    manifest: &ComponentManifest,
    manifest_bytes: u64,
) -> bool {
    validate_runtime_manifest_contract(
        component,
        manifest,
        manifest_bytes,
        MAX_RUNTIME_TREE_ENTRIES,
        MAX_RUNTIME_TREE_TOTAL_BYTES,
        1,
    )
    .is_ok()
}

#[derive(Clone, Copy)]
struct RuntimeManifestAdmission {
    download_bytes: u64,
}

fn validate_runtime_manifest_contract(
    component: &RuntimeId,
    manifest: &ComponentManifest,
    manifest_bytes: u64,
    max_entries: usize,
    max_bytes: u64,
    download_concurrency: usize,
) -> Result<RuntimeManifestAdmission, JavaRuntimeLookupError> {
    if manifest.files.len() > max_entries {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            "runtime manifest exceeds the entry bound",
        ));
    }
    let contract_root = Path::new("runtime");
    let mut declared_paths = HashSet::new();
    let mut canonical_prefixes = HashMap::new();
    let mut reserved_paths = HashSet::from([
        PortableRelativePath::new(COMPONENT_MANIFEST_PROOF_FILE)
            .expect("fixed component manifest proof name")
            .key(),
        PortableRelativePath::new(".axial-ready")
            .expect("fixed runtime ready marker")
            .key(),
    ]);
    let mut filesystem_entries = HashMap::new();
    let mut collision_entries = HashMap::new();
    let mut link_targets = Vec::new();
    for reserved_path in &reserved_paths {
        let inserted = insert_runtime_tree_node(
            &mut collision_entries,
            PathBuf::from(reserved_path.as_str()),
            RuntimeTreeNodeKind::File,
        );
        debug_assert!(inserted, "fixed runtime paths have distinct topology");
    }
    let mut raw_total = 0_u64;
    let mut compressed_total = 0_u64;
    let mut download_total = 0_u64;
    let mut file_entries = 0_usize;
    let mut lzma_entries = 0_usize;
    for (relative_path, file) in &manifest.files {
        let (destination, filesystem_key) =
            component_manifest_destination_with_key(component, contract_root, relative_path)?;
        let normalized_relative = destination
            .strip_prefix(contract_root)
            .expect("validated runtime destination remains below its contract root")
            .to_path_buf();
        if normalized_relative.components().count() > MAX_RUNTIME_TREE_DEPTH {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest exceeds the depth bound",
            ));
        }
        if !record_runtime_manifest_prefix_spellings(
            &mut canonical_prefixes,
            &normalized_relative,
            &filesystem_key,
        ) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest contains aliased path-prefix spellings",
            ));
        }
        if !declared_paths.insert(filesystem_key.clone()) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest contains colliding paths",
            ));
        }
        if reserved_paths.contains(&filesystem_key) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest path collides with runtime-owned state",
            ));
        }

        let mut transient_paths = Vec::new();
        let kind = match file.kind.as_str() {
            "directory" => {
                if file.downloads.is_some() || file.target.is_some() {
                    return Err(runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime directory contains incompatible metadata",
                    ));
                }
                RuntimeTreeNodeKind::Directory
            }
            "link" => {
                if file.downloads.is_some() {
                    return Err(runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime link contains incompatible metadata",
                    ));
                }
                let target = file.target.as_deref().ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime manifest link is missing its target",
                    )
                })?;
                if target.len() > MAX_RUNTIME_LINK_TARGET_BYTES {
                    return Err(runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::PolicyRejected,
                        "runtime manifest link target exceeds the length bound",
                    ));
                }
                let resolved_target = component_manifest_link_target_path(
                    component,
                    contract_root,
                    &destination,
                    relative_path,
                    target,
                )?;
                link_targets.push((normalized_relative.clone(), resolved_target));
                RuntimeTreeNodeKind::Link
            }
            "file" => {
                if file.target.is_some() {
                    return Err(runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime file contains incompatible link metadata",
                    ));
                }
                let downloads = file.downloads.as_ref().ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime file is missing exact download proof",
                    )
                })?;
                let raw = downloads.raw.as_ref().ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::MetadataInvalid,
                        "runtime file is missing exact raw proof",
                    )
                })?;
                let raw_size = exact_runtime_download_size(component, raw, "raw")?;
                raw_total = raw_total.checked_add(raw_size).ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::PolicyRejected,
                        "runtime manifest byte total overflowed",
                    )
                })?;
                let selected_size = if let Some(lzma) = downloads.lzma.as_ref() {
                    let compressed_size =
                        exact_runtime_download_size(component, lzma, "compressed")?;
                    compressed_total =
                        compressed_total
                            .checked_add(compressed_size)
                            .ok_or_else(|| {
                                runtime_source_failure(
                                    component,
                                    RuntimeSourceFailureKind::PolicyRejected,
                                    "runtime manifest byte total overflowed",
                                )
                            })?;
                    lzma_entries = lzma_entries.checked_add(1).ok_or_else(|| {
                        runtime_source_failure(
                            component,
                            RuntimeSourceFailureKind::PolicyRejected,
                            "runtime manifest entry total overflowed",
                        )
                    })?;
                    transient_paths.push(
                        PortableRelativePath::new(&format!("{filesystem_key}.axial-tmp.lzma"))
                            .map_err(|_| {
                                runtime_source_failure(
                                    component,
                                    RuntimeSourceFailureKind::PolicyRejected,
                                    "runtime manifest path leaves no room for staging",
                                )
                            })?
                            .key(),
                    );
                    compressed_size
                } else {
                    raw_size
                };
                transient_paths.push(
                    PortableRelativePath::new(&format!("{filesystem_key}.axial-tmp"))
                        .map_err(|_| {
                            runtime_source_failure(
                                component,
                                RuntimeSourceFailureKind::PolicyRejected,
                                "runtime manifest path leaves no room for staging",
                            )
                        })?
                        .key(),
                );
                download_total = download_total.checked_add(selected_size).ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::PolicyRejected,
                        "runtime manifest byte total overflowed",
                    )
                })?;
                file_entries = file_entries.checked_add(1).ok_or_else(|| {
                    runtime_source_failure(
                        component,
                        RuntimeSourceFailureKind::PolicyRejected,
                        "runtime manifest entry total overflowed",
                    )
                })?;
                RuntimeTreeNodeKind::File
            }
            _ => {
                return Err(runtime_source_failure(
                    component,
                    RuntimeSourceFailureKind::MetadataInvalid,
                    "runtime manifest contains an unsupported entry",
                ));
            }
        };
        if !insert_runtime_tree_node(
            &mut filesystem_entries,
            PathBuf::from(filesystem_key.as_str()),
            kind,
        ) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest contains an invalid path topology",
            ));
        }
        if !insert_runtime_tree_node(
            &mut collision_entries,
            PathBuf::from(filesystem_key.as_str()),
            kind,
        ) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest contains a filesystem path collision",
            ));
        }
        for transient_path in transient_paths {
            if declared_paths.contains(&transient_path)
                || !reserved_paths.insert(transient_path.clone())
                || !insert_runtime_tree_node(
                    &mut collision_entries,
                    PathBuf::from(transient_path.as_str()),
                    RuntimeTreeNodeKind::File,
                )
            {
                return Err(runtime_source_failure(
                    component,
                    RuntimeSourceFailureKind::PolicyRejected,
                    "runtime manifest path collides with runtime-owned state",
                ));
            }
        }
    }
    for (link_destination, target) in link_targets {
        let target_relative = target.strip_prefix(contract_root).map_err(|_| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest link target has invalid topology",
            )
        })?;
        let target_relative = target_relative.to_str().ok_or_else(|| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest link target has invalid topology",
            )
        })?;
        let (canonical_target, target_key) =
            component_manifest_destination_with_key(component, Path::new(""), target_relative)?;
        let target_kind = filesystem_entries.get(&PathBuf::from(target_key.as_str()));
        let target_is_link_parent_ancestor = target_kind == Some(&RuntimeTreeNodeKind::Directory)
            && link_destination
                .parent()
                .is_some_and(|parent| parent.starts_with(&canonical_target));
        if !runtime_manifest_prefix_spellings_match(
            &canonical_prefixes,
            &canonical_target,
            &target_key,
        ) || !target_kind.is_some_and(|kind| *kind != RuntimeTreeNodeKind::Link)
            || target_is_link_parent_ancestor
        {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest link target has invalid topology",
            ));
        }
    }
    let admitted_total = raw_total
        .checked_add(compressed_total)
        .and_then(|total| total.checked_add(manifest_bytes))
        .and_then(|total| total.checked_add(64))
        .ok_or_else(|| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest byte total overflowed",
            )
        })?;
    let concurrent_files = file_entries.min(download_concurrency.max(1));
    let concurrent_lzma = lzma_entries.min(download_concurrency.max(1));
    let transient_entries = 2_usize
        .checked_add(concurrent_files)
        .and_then(|entries| entries.checked_add(concurrent_lzma))
        .ok_or_else(|| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest entry total overflowed",
            )
        })?;
    let peak_entries = filesystem_entries
        .len()
        .checked_add(transient_entries)
        .ok_or_else(|| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest entry total overflowed",
            )
        })?;
    if peak_entries > max_entries || raw_total > max_bytes || admitted_total > max_bytes {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            "runtime manifest exceeds the aggregate bound",
        ));
    }
    Ok(RuntimeManifestAdmission {
        download_bytes: download_total,
    })
}

#[cfg(test)]
pub(super) fn validate_ephemeral_processor_manifest_for_test(
    manifest: &ComponentManifest,
    manifest_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    validate_runtime_manifest_contract(
        &RuntimeId::from("java-runtime-gamma"),
        manifest,
        manifest_bytes,
        MAX_RUNTIME_TREE_ENTRIES,
        MAX_RUNTIME_TREE_TOTAL_BYTES,
        1,
    )
    .map(|_| ())
}

fn exact_runtime_download_size(
    component: &RuntimeId,
    download: &ComponentManifestDownload,
    label: &str,
) -> Result<u64, JavaRuntimeLookupError> {
    if download.url.trim().is_empty() {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!("runtime {label} file is missing its source URL"),
        ));
    }
    let size = download.size.filter(|size| *size > 0).ok_or_else(|| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!("runtime {label} file is missing exact size"),
        )
    })?;
    if size > MAX_RUNTIME_FILE_BYTES {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            format!("runtime {label} file exceeds the per-file bound"),
        ));
    }
    if !download.sha1.as_deref().is_some_and(runtime_sha1_is_valid) {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!("runtime {label} file is missing exact checksum"),
        ));
    }
    Ok(size)
}

fn persist_component_manifest_proof(
    temp_dir: &ManagedDir,
    component_manifest: &ComponentManifest,
) -> Result<(), JavaRuntimeLookupError> {
    let bytes = component_manifest_proof_bytes(component_manifest)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    temp_dir
        .write_new_exact(COMPONENT_MANIFEST_PROOF_FILE, &bytes)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

#[cfg(test)]
pub(super) async fn install_runtime_manifest_files(
    component: &RuntimeId,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    let managed = ManagedDir::open_root(temp_dir)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let admitted_download_bytes =
        files
            .values()
            .filter(|file| file.kind == "file")
            .try_fold(0_u64, |total, file| {
                total
                    .checked_add(runtime_manifest_file_download_bytes(component, file)?)
                    .ok_or_else(|| {
                        JavaRuntimeLookupError::Install(
                            "runtime manifest download byte total overflowed".to_string(),
                        )
                    })
            })?;
    let (_cancellation_sender, mut cancellation) = runtime_cancellation_channel();
    install_runtime_manifest_files_with_concurrency(
        component,
        &managed,
        temp_dir,
        files,
        observer,
        runtime_file_download_concurrency(),
        admitted_download_bytes,
        &mut cancellation,
    )
    .await
}

async fn install_runtime_manifest_files_with_concurrency(
    component: &RuntimeId,
    temp_dir: &ManagedDir,
    projection: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
    admitted_download_bytes: u64,
    cancellation: &mut RuntimeCancellation,
) -> Result<(), JavaRuntimeLookupError> {
    let plan = plan_runtime_manifest_files(files);

    for (relative_path, file) in plan.directory_entries.into_iter().chain(plan.other_entries) {
        let mut entry_cancellation = RuntimeCancellationSet::single(cancellation.clone());
        install_runtime_manifest_file_until_cancelled(
            component,
            temp_dir,
            projection,
            &relative_path,
            file,
            None,
            &mut entry_cancellation,
        )
        .await?;
    }

    let total_files = plan.file_entries.len() + plan.link_entries.len();
    let total_bytes = admitted_download_bytes;
    if total_files > 0 {
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.as_str().to_string(),
            current: 0,
            total: total_files,
            bytes_done: 0,
            bytes_total: total_bytes,
        });
    }

    let (lane_cancellation_sender, lane_cancellation) = runtime_cancellation_channel();
    let source_urls = plan
        .file_entries
        .iter()
        .flat_map(|(_, file)| {
            file.downloads.as_ref().into_iter().flat_map(|downloads| {
                downloads
                    .raw
                    .iter()
                    .chain(downloads.lzma.iter())
                    .map(|download| download.url.as_str())
            })
        })
        .collect::<Vec<_>>();
    let download_client = if source_urls.is_empty() {
        None
    } else {
        Some(runtime_transfer_client(component, source_urls)?)
    };
    let mut file_downloads =
        futures_util::stream::iter(plan.file_entries.into_iter().map(|entry| {
            let download_client = download_client.clone();
            let temp_dir = temp_dir.clone();
            let projection = projection.to_path_buf();
            let component = component.clone();
            let mut cancellation =
                RuntimeCancellationSet::pair(cancellation.clone(), lane_cancellation.clone());
            async move {
                let (relative_path, file) = entry;
                let bytes = runtime_manifest_file_download_bytes(&component, &file)?;
                Box::pin(install_runtime_manifest_file_until_cancelled(
                    &component,
                    &temp_dir,
                    &projection,
                    &relative_path,
                    file,
                    download_client,
                    &mut cancellation,
                ))
                .await?;
                Ok::<CompletedRuntimeManifestFile, JavaRuntimeLookupError>(
                    CompletedRuntimeManifestFile { bytes },
                )
            }
        }))
        .buffer_unordered(download_concurrency.max(1));

    let mut completed_files = 0;
    let mut completed_bytes = 0_u64;
    let mut first_error = None;
    while let Some(result) = file_downloads.next().await {
        match result {
            Ok(completed) if first_error.is_none() && !cancellation.is_cancelled() => {
                completed_files += 1;
                let Some(next_completed_bytes) = completed_bytes.checked_add(completed.bytes)
                else {
                    first_error = Some(JavaRuntimeLookupError::Install(
                        "runtime download progress byte total overflowed".to_string(),
                    ));
                    lane_cancellation_sender.cancel();
                    continue;
                };
                completed_bytes = next_completed_bytes;
                observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
                    component: component.as_str().to_string(),
                    current: completed_files,
                    total: total_files,
                    bytes_done: completed_bytes,
                    bytes_total: total_bytes,
                });
            }
            Ok(_) => {}
            Err(error) => {
                if first_error.is_none() && !cancellation.is_cancelled() {
                    first_error = Some(error);
                }
                lane_cancellation_sender.cancel();
            }
        }
        if cancellation.is_cancelled() {
            lane_cancellation_sender.cancel();
        }
    }
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }
    if let Some(error) = first_error {
        return Err(error);
    }

    for (relative_path, file) in plan.link_entries {
        let mut link_cancellation = RuntimeCancellationSet::single(cancellation.clone());
        install_runtime_manifest_file_until_cancelled(
            component,
            temp_dir,
            projection,
            &relative_path,
            file,
            None,
            &mut link_cancellation,
        )
        .await?;
        completed_files += 1;
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.as_str().to_string(),
            current: completed_files,
            total: total_files,
            bytes_done: completed_bytes,
            bytes_total: total_bytes,
        });
    }

    Ok(())
}

pub(super) struct CompletedRuntimeManifestFile {
    pub(super) bytes: u64,
}

fn runtime_manifest_file_download_bytes(
    component: &RuntimeId,
    file: &ComponentManifestFile,
) -> Result<u64, JavaRuntimeLookupError> {
    file.downloads
        .as_ref()
        .and_then(|downloads| downloads.lzma.as_ref().or(downloads.raw.as_ref()))
        .and_then(|raw| raw.size)
        .ok_or_else(|| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::MetadataInvalid,
                "runtime file is missing its admitted download size",
            )
        })
}

#[derive(Debug, Default)]
pub(crate) struct RuntimeManifestInstallPlan {
    pub(crate) directory_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) file_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) link_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) other_entries: Vec<(String, ComponentManifestFile)>,
}

pub(crate) fn plan_runtime_manifest_files(
    files: HashMap<String, ComponentManifestFile>,
) -> RuntimeManifestInstallPlan {
    let mut entries = files.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut plan = RuntimeManifestInstallPlan::default();
    for (relative_path, file) in entries {
        match file.kind.as_str() {
            "directory" => plan.directory_entries.push((relative_path, file)),
            "file" => plan.file_entries.push((relative_path, file)),
            "link" => plan.link_entries.push((relative_path, file)),
            _ => plan.other_entries.push((relative_path, file)),
        }
    }

    plan
}

#[cfg(test)]
pub(super) async fn install_runtime_manifest_file(
    component: &RuntimeId,
    temp_dir: &Path,
    relative_path: &str,
    file: ComponentManifestFile,
) -> Result<(), JavaRuntimeLookupError> {
    let urls = file
        .downloads
        .as_ref()
        .into_iter()
        .flat_map(|downloads| {
            downloads
                .raw
                .iter()
                .chain(downloads.lzma.iter())
                .map(|download| download.url.as_str())
        })
        .collect::<Vec<_>>();
    let client = (!urls.is_empty())
        .then(|| runtime_transfer_client(component, urls))
        .transpose()?;
    let managed = ManagedDir::open_root(temp_dir)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let (_cancellation_sender, cancellation) = runtime_cancellation_channel();
    let mut cancellation = RuntimeCancellationSet::single(cancellation);
    install_runtime_manifest_file_until_cancelled(
        component,
        &managed,
        temp_dir,
        relative_path,
        file,
        client,
        &mut cancellation,
    )
    .await
}

async fn install_runtime_manifest_file_until_cancelled(
    component: &RuntimeId,
    temp_dir: &ManagedDir,
    projection: &Path,
    relative_path: &str,
    file: ComponentManifestFile,
    download_client: Option<crate::download::TransferClient>,
    cancellation: &mut RuntimeCancellationSet,
) -> Result<(), JavaRuntimeLookupError> {
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }
    let relative = PortableRelativePath::new_exact(relative_path).map_err(|_| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            format!(
                "unsafe runtime manifest path: {}",
                bounded_manifest_file_label(relative_path)
            ),
        )
    })?;
    let destination = component_manifest_destination(component, projection, relative_path)?;
    if file.kind == "directory" {
        temp_dir
            .open_or_create_relative_directory(&relative)
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        if cancellation.is_cancelled() {
            return Err(runtime_materialization_cancelled());
        }
        return Ok(());
    }
    if file.kind == "link" {
        return install_runtime_manifest_link(
            component,
            temp_dir,
            projection,
            &destination,
            relative_path,
            &file,
            cancellation,
        )
        .await;
    }
    if file.kind != "file" {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!(
                "unsupported runtime manifest entry {} ({})",
                bounded_manifest_file_label(relative_path),
                file.kind
            ),
        ));
    }
    let RuntimeFileDownloadSelection { raw, lzma } =
        select_runtime_file_downloads(component, relative_path, file.downloads)?;

    let client = download_client.ok_or_else(|| {
        JavaRuntimeLookupError::Install("runtime transfer client is absent".to_string())
    })?;
    let raw_size = raw.size.ok_or_else(|| {
        JavaRuntimeLookupError::Install("runtime raw file size is absent".to_string())
    })?;
    let raw_sha1 = raw
        .sha1
        .as_deref()
        .and_then(runtime_sha1_bytes)
        .ok_or_else(|| {
            JavaRuntimeLookupError::Install("runtime raw file digest is invalid".to_string())
        })?;
    if let Some(lzma) = lzma {
        let source = fetch_runtime_source_until_cancelled(
            component,
            temp_dir,
            client,
            &lzma.url,
            RuntimeDownloadEvidence::from(&lzma),
            relative_path,
            cancellation,
        )
        .await?;
        let bytes = decompress_lzma_runtime_source(
            component.clone(),
            temp_dir.clone(),
            source,
            RuntimeDownloadEvidence::from(&raw),
            relative_path.to_string(),
            cancellation.thread_cancellation(),
        )
        .await?;
        temp_dir
            .import_relative_authenticated(
                &relative,
                std::io::Cursor::new(bytes),
                raw_size,
                raw_sha1,
            )
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    } else {
        let source = fetch_runtime_source_until_cancelled(
            component,
            temp_dir,
            client,
            &raw.url,
            RuntimeDownloadEvidence::from(&raw),
            relative_path,
            cancellation,
        )
        .await?;
        let (source, authority) = source.into_parts();
        temp_dir
            .import_verified_source_relative(&relative, source, authority, raw_size, raw_sha1)
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }
    #[cfg(unix)]
    if file.executable {
        temp_dir
            .make_file_executable_relative(&relative)
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }
    if cancellation.is_cancelled() {
        return Err(runtime_materialization_cancelled());
    }

    Ok(())
}

struct RuntimeFileDownloadSelection {
    raw: ComponentManifestDownload,
    lzma: Option<ComponentManifestDownload>,
}

fn select_runtime_file_downloads(
    component: &RuntimeId,
    relative_path: &str,
    downloads: Option<ComponentManifestDownloads>,
) -> Result<RuntimeFileDownloadSelection, JavaRuntimeLookupError> {
    let Some(downloads) = downloads else {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!(
                "runtime manifest file {} is missing download proof",
                bounded_manifest_file_label(relative_path)
            ),
        ));
    };
    let Some(raw) = downloads.raw else {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!(
                "runtime manifest file {} is missing download proof",
                bounded_manifest_file_label(relative_path)
            ),
        ));
    };
    validate_runtime_download_checksum(component, relative_path, &raw, "file")?;
    if let Some(lzma) = downloads.lzma.as_ref() {
        validate_runtime_download_checksum(component, relative_path, lzma, "lzma file")?;
    }
    Ok(RuntimeFileDownloadSelection {
        raw,
        lzma: downloads.lzma,
    })
}

fn validate_runtime_download_checksum(
    component: &RuntimeId,
    relative_path: &str,
    download: &ComponentManifestDownload,
    label: &str,
) -> Result<(), JavaRuntimeLookupError> {
    if download.sha1.as_deref().is_some_and(runtime_sha1_is_valid) {
        return Ok(());
    }
    Err(runtime_source_failure(
        component,
        RuntimeSourceFailureKind::MetadataInvalid,
        format!(
            "runtime manifest {label} {} is missing checksum proof",
            bounded_manifest_file_label(relative_path)
        ),
    ))
}

fn runtime_cancellation_io_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "runtime staging was cancelled",
    )
}

#[cfg(test)]
struct DecompressionTestHook {
    output_path: PathBuf,
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static DECOMPRESSION_TEST_HOOK: std::sync::OnceLock<
    std::sync::Mutex<Option<DecompressionTestHook>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) struct DecompressionTestGate {
    pub(super) started: std::sync::mpsc::Receiver<()>,
    pub(super) release: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
pub(super) fn block_runtime_decompression_for_test(output_path: PathBuf) -> DecompressionTestGate {
    let (started_tx, started) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut hook = DECOMPRESSION_TEST_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("runtime decompression test hook lock");
    assert!(
        hook.is_none(),
        "runtime decompression test hook already armed"
    );
    *hook = Some(DecompressionTestHook {
        output_path,
        started: started_tx,
        release: release_rx,
    });
    DecompressionTestGate { started, release }
}

#[cfg(test)]
fn wait_for_decompression_test_release(output_path: &Path) {
    let mut armed = DECOMPRESSION_TEST_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("runtime decompression test hook lock");
    let hook = armed
        .as_ref()
        .is_some_and(|hook| hook.output_path == output_path)
        .then(|| armed.take().expect("matching decompression test hook"));
    drop(armed);
    if let Some(hook) = hook {
        let _ = hook.started.send(());
        let _ = hook.release.recv();
    }
}

#[cfg(not(test))]
fn wait_for_decompression_test_release(_output_path: &Path) {}

async fn decompress_lzma_runtime_source(
    component: RuntimeId,
    destination_root: ManagedDir,
    source: RuntimeVerifiedSource,
    expected: RuntimeDownloadEvidence,
    relative_path: String,
    cancellation: RuntimeThreadCancellation,
) -> Result<Vec<u8>, JavaRuntimeLookupError> {
    let hook_path = destination_root.path().join(&relative_path);
    let (source, authority) = source.into_parts();
    let (result, discard) = tokio::task::spawn_blocking(move || {
        wait_for_decompression_test_release(&hook_path);
        if cancellation.is_cancelled() {
            return (Err(runtime_materialization_cancelled()), source.discard());
        }
        let mut input =
            RuntimeInstallReader::with_cancellation(BufReader::new(source), cancellation.clone());
        let capacity = expected
            .size
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(0);
        let mut output = RuntimeIntegrityWriter::with_cancellation(
            Vec::with_capacity(capacity),
            component.clone(),
            expected.clone(),
            &relative_path,
            cancellation,
        );
        let decompressed = decompress_lzma_stream(&component, &mut input, &mut output);
        let flushed = output
            .flush()
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()));
        let RuntimeIntegrityWriter {
            output: bytes,
            hasher,
            size,
            ..
        } = output;
        let actual = RuntimeDownloadActual {
            size,
            sha1: format!("{:x}", hasher.finalize()),
        };
        let verified = decompressed.and(flushed).and_then(|()| {
            verify_runtime_download(&relative_path, &expected, &actual).map_err(|error| {
                runtime_source_failure(
                    &component,
                    RuntimeSourceFailureKind::IntegrityMismatch,
                    error.to_string(),
                )
            })
        });
        let source = input.input.into_inner();
        (verified.map(|()| bytes), source.discard())
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    match discard {
        crate::download::VerifiedTransferDiscardOutcome::Discarded {
            authority: terminal,
            ..
        } if terminal.shares_retained_authority(&authority) => {
            destination_root
                .settle()
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        }
        crate::download::VerifiedTransferDiscardOutcome::Discarded { .. } => {
            return Err(JavaRuntimeLookupError::Install(
                "runtime decompression returned unrelated authority".to_string(),
            ));
        }
        crate::download::VerifiedTransferDiscardOutcome::Pending(obligation) => {
            destination_root
                .retain_verified_transfer_discard(obligation, authority)
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
            return Err(JavaRuntimeLookupError::Install(
                "runtime compressed source discard remains unsettled".to_string(),
            ));
        }
    }
    result
}

fn decompress_lzma_stream<R: BufRead, W: Write>(
    component: &RuntimeId,
    input: &mut RuntimeInstallReader<R>,
    output: &mut RuntimeIntegrityWriter<W>,
) -> Result<(), JavaRuntimeLookupError> {
    if let Err(error) = lzma_rs::lzma_decompress(input, output) {
        return Err(output
            .take_failure()
            .or_else(|| input.take_failure())
            .unwrap_or_else(|| {
                runtime_source_failure(
                    component,
                    RuntimeSourceFailureKind::IntegrityMismatch,
                    error.to_string(),
                )
            }));
    }
    Ok(())
}

struct RuntimeInstallReader<R> {
    input: R,
    failure: Option<String>,
    cancellation: Option<RuntimeThreadCancellation>,
}

impl<R> RuntimeInstallReader<R> {
    #[cfg(test)]
    fn new(input: R) -> Self {
        Self {
            input,
            failure: None,
            cancellation: None,
        }
    }

    fn with_cancellation(input: R, cancellation: RuntimeThreadCancellation) -> Self {
        Self {
            input,
            failure: None,
            cancellation: Some(cancellation),
        }
    }

    fn take_failure(&mut self) -> Option<JavaRuntimeLookupError> {
        self.failure.take().map(JavaRuntimeLookupError::Install)
    }
}

impl<R: Read> Read for RuntimeInstallReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(RuntimeThreadCancellation::is_cancelled)
        {
            let error = runtime_cancellation_io_error();
            self.failure.get_or_insert_with(|| error.to_string());
            return Err(error);
        }
        self.input.read(buffer).inspect_err(|error| {
            self.failure.get_or_insert_with(|| error.to_string());
        })
    }
}

impl<R: BufRead> BufRead for RuntimeInstallReader<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(RuntimeThreadCancellation::is_cancelled)
        {
            let error = runtime_cancellation_io_error();
            self.failure.get_or_insert_with(|| error.to_string());
            return Err(error);
        }
        match self.input.fill_buf() {
            Ok(buffer) => Ok(buffer),
            Err(error) => {
                self.failure.get_or_insert_with(|| error.to_string());
                Err(error)
            }
        }
    }

    fn consume(&mut self, amount: usize) {
        self.input.consume(amount);
    }
}

struct RuntimeIntegrityWriter<W> {
    output: W,
    component: RuntimeId,
    expected: RuntimeDownloadEvidence,
    relative_path: String,
    hasher: Sha1,
    size: u64,
    failure: Option<JavaRuntimeLookupError>,
    cancellation: Option<RuntimeThreadCancellation>,
}

impl<W> RuntimeIntegrityWriter<W> {
    #[cfg(test)]
    fn new(
        output: W,
        component: RuntimeId,
        expected: RuntimeDownloadEvidence,
        relative_path: &str,
    ) -> Self {
        Self {
            output,
            component,
            expected,
            relative_path: relative_path.to_string(),
            hasher: Sha1::new(),
            size: 0,
            failure: None,
            cancellation: None,
        }
    }

    fn with_cancellation(
        output: W,
        component: RuntimeId,
        expected: RuntimeDownloadEvidence,
        relative_path: &str,
        cancellation: RuntimeThreadCancellation,
    ) -> Self {
        Self {
            output,
            component,
            expected,
            relative_path: relative_path.to_string(),
            hasher: Sha1::new(),
            size: 0,
            failure: None,
            cancellation: Some(cancellation),
        }
    }

    fn take_failure(&mut self) -> Option<JavaRuntimeLookupError> {
        self.failure.take()
    }
}

impl<W: Write> Write for RuntimeIntegrityWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(RuntimeThreadCancellation::is_cancelled)
        {
            let error = runtime_cancellation_io_error();
            self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
            return Err(error);
        }
        let next_size = self.size.saturating_add(buffer.len() as u64);
        if let Some(expected_size) = self.expected.size
            && next_size > expected_size
        {
            let message = super::file_download::RuntimeDownloadIntegrityError::SizeMismatch {
                file: bounded_manifest_file_label(&self.relative_path),
                expected: expected_size,
                actual: next_size,
            }
            .to_string();
            self.failure = Some(runtime_source_failure(
                &self.component,
                RuntimeSourceFailureKind::IntegrityMismatch,
                message.clone(),
            ));
            return Err(std::io::Error::other(message));
        }
        let written = match self.output.write(buffer) {
            Ok(0) if !buffer.is_empty() => {
                let error = std::io::Error::from(std::io::ErrorKind::WriteZero);
                self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
                return Err(error);
            }
            Ok(written) => written,
            Err(error) => {
                self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
                return Err(error);
            }
        };
        self.hasher.update(&buffer[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(RuntimeThreadCancellation::is_cancelled)
        {
            let error = runtime_cancellation_io_error();
            self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
            return Err(error);
        }
        self.output.flush().inspect_err(|error| {
            self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
        })
    }
}

async fn install_runtime_manifest_link(
    component: &RuntimeId,
    temp_dir: &ManagedDir,
    projection: &Path,
    destination: &Path,
    relative_path: &str,
    file: &ComponentManifestFile,
    cancellation: &RuntimeCancellationSet,
) -> Result<(), JavaRuntimeLookupError> {
    let Some(target) = file.target.as_deref() else {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!(
                "runtime manifest link {} is missing target",
                bounded_manifest_file_label(relative_path)
            ),
        ));
    };
    component_manifest_link_target_path(component, projection, destination, relative_path, target)?;
    let relative = PortableRelativePath::new_exact(relative_path).map_err(|_| {
        JavaRuntimeLookupError::Install("runtime manifest link path is invalid".to_string())
    })?;
    install_runtime_manifest_symlink(
        temp_dir.clone(),
        relative,
        target.to_string(),
        cancellation.thread_cancellation(),
    )
    .await
}

#[cfg(unix)]
async fn install_runtime_manifest_symlink(
    destination_root: ManagedDir,
    relative: PortableRelativePath,
    target: String,
    cancellation: RuntimeThreadCancellation,
) -> Result<(), JavaRuntimeLookupError> {
    tokio::task::spawn_blocking(move || {
        if cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "runtime staging was cancelled",
            ));
        }
        destination_root
            .create_owned_symlink_relative(&relative, &target)
            .map_err(runtime_loader_io)
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

#[cfg(not(unix))]
async fn install_runtime_manifest_symlink(
    _destination_root: ManagedDir,
    _relative: PortableRelativePath,
    _target: String,
    _cancellation: RuntimeThreadCancellation,
) -> Result<(), JavaRuntimeLookupError> {
    Err(JavaRuntimeLookupError::Install(
        "runtime manifest link entries are unsupported on this platform".to_string(),
    ))
}

fn runtime_sha1_is_valid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn runtime_sha1_bytes(value: &str) -> Option<[u8; 20]> {
    if !runtime_sha1_is_valid(value) {
        return None;
    }
    let mut digest = [0_u8; 20];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16)? as u8;
        let low = (pair[1] as char).to_digit(16)? as u8;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

#[cfg(test)]
mod lzma_failure_classification_tests {
    use super::{
        JavaRuntimeLookupError, RuntimeDownloadEvidence, RuntimeId, RuntimeInstallReader,
        RuntimeIntegrityWriter, RuntimeSourceFailureKind, decompress_lzma_stream,
    };
    use std::io::{BufRead, Cursor, Read, Write};

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn evidence(size: u64) -> RuntimeDownloadEvidence {
        RuntimeDownloadEvidence {
            size: Some(size),
            sha1: None,
        }
    }

    fn compressed_fixture() -> Vec<u8> {
        let mut compressed = Vec::new();
        lzma_rs::lzma_compress(&mut Cursor::new(b"runtime bytes"), &mut compressed)
            .expect("compress runtime fixture");
        compressed
    }

    #[test]
    fn lzma_input_io_failure_stays_local_install_failure() {
        let component = RuntimeId::from("java-runtime-gamma");
        let mut input = RuntimeInstallReader::new(FailingReader);
        let mut output =
            RuntimeIntegrityWriter::new(Vec::new(), component.clone(), evidence(13), "bin/java");

        assert!(matches!(
            decompress_lzma_stream(&component, &mut input, &mut output),
            Err(JavaRuntimeLookupError::Install(_))
        ));
    }

    #[test]
    fn lzma_output_io_failure_stays_local_install_failure() {
        let component = RuntimeId::from("java-runtime-gamma");
        let mut input = RuntimeInstallReader::new(Cursor::new(compressed_fixture()));
        let mut output =
            RuntimeIntegrityWriter::new(FailingWriter, component.clone(), evidence(13), "bin/java");

        assert!(matches!(
            decompress_lzma_stream(&component, &mut input, &mut output),
            Err(JavaRuntimeLookupError::Install(_))
        ));
    }

    #[test]
    fn invalid_lzma_bytes_stay_runtime_source_failure() {
        let component = RuntimeId::from("java-runtime-gamma");
        let mut input = RuntimeInstallReader::new(Cursor::new(b"not lzma"));
        let mut output =
            RuntimeIntegrityWriter::new(Vec::new(), component.clone(), evidence(13), "bin/java");

        assert!(matches!(
            decompress_lzma_stream(&component, &mut input, &mut output),
            Err(JavaRuntimeLookupError::RuntimeSource(failure))
                if failure.component() == &component
                    && failure.kind() == RuntimeSourceFailureKind::IntegrityMismatch
        ));
    }
}

#[cfg(test)]
mod quarantine_observation_tests {
    use super::{
        ManagedRuntimeQuarantineObligation, ManagedRuntimeQuarantineObservation,
        RuntimePathObservation,
    };
    use crate::runtime::{ManagedRuntimeCache, RuntimeId};

    #[test]
    fn quarantine_obligation_is_omitted_only_for_confirmed_absence() {
        let absent = RuntimePathObservation::Absent;
        assert_eq!(
            ManagedRuntimeQuarantineObservation::from(absent),
            ManagedRuntimeQuarantineObservation::Absent
        );
        assert!(!absent.is_present());
        assert!(!absent.retains_obligation());

        assert!(RuntimePathObservation::Present.is_present());
        assert!(RuntimePathObservation::Present.retains_obligation());
        assert_eq!(
            ManagedRuntimeQuarantineObservation::from(RuntimePathObservation::Present),
            ManagedRuntimeQuarantineObservation::Present
        );
        let indeterminate = RuntimePathObservation::Indeterminate;
        assert_eq!(
            ManagedRuntimeQuarantineObservation::from(indeterminate),
            ManagedRuntimeQuarantineObservation::Indeterminate
        );
        assert!(!indeterminate.is_present());
        assert!(indeterminate.retains_obligation());
    }

    #[test]
    fn retained_quarantine_observation_is_closed_and_path_free() {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("Runtime cache");
        let component = RuntimeId::from("jre-legacy");
        let quarantine = cache
            .component_root(component.as_str())
            .expect("Runtime root")
            .with_file_name("jre-legacy.quarantine");
        std::fs::create_dir(&quarantine).expect("quarantine fixture");
        let obligation = ManagedRuntimeQuarantineObligation { cache, component };

        assert_eq!(
            obligation.observation(),
            ManagedRuntimeQuarantineObservation::Present
        );
        assert_eq!(format!("{:?}", obligation.observation()), "Present");

        std::fs::remove_dir(&quarantine).expect("remove quarantine fixture");
        assert_eq!(
            obligation.observation(),
            ManagedRuntimeQuarantineObservation::Absent
        );
        assert_eq!(format!("{:?}", obligation.observation()), "Absent");
    }
}
