use super::file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, bounded_manifest_file_label,
    component_manifest_destination, component_manifest_destination_with_key,
    component_manifest_link_target_path, fetch_runtime_file, runtime_download_client,
    runtime_download_temp_path, runtime_file_download_concurrency, runtime_filesystem_path,
    verify_runtime_download,
};
use super::layout::{ManagedRuntimeCache, java_executable, runtime_executable_ready};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, ComponentManifestDownload,
    ComponentManifestDownloads, ComponentManifestFile, RuntimeSourceReceipt,
    component_manifest_proof_bytes,
};
use super::model::{
    JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeId, RuntimeSourceFailure,
    RuntimeSourceFailureKind,
};
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodInventory, KnownGoodRoot,
    known_good_link_target_matches,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use tokio::fs as async_fs;

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
    _file_lock: RuntimeInstallFileLock,
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
    Effect(ManagedRuntimeFailureReceipt),
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

    pub async fn revalidate(
        &self,
        cache: &ManagedRuntimeCache,
        expected_component: &RuntimeId,
    ) -> bool {
        if !self.matches_cache(cache) || &self.component != expected_component {
            return false;
        }
        let (Some(source), Some(install_root)) = (
            self.source.as_ref(),
            cache.component_root(expected_component.as_str()),
        ) else {
            return false;
        };
        source.component() == expected_component
            && runtime_tree_matches_source(&install_root, source.as_ref()).await
            && self
                .quarantine
                .as_ref()
                .is_none_or(|obligation| obligation.path_observation().is_present())
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
        runtime_tree_matches_source(&install_root, source).await
            && self
                .quarantine
                .as_ref()
                .is_none_or(|obligation| obligation.path_observation().is_present())
    }

    pub(crate) fn into_source_receipt(self) -> RuntimeSourceReceipt {
        let Self {
            source,
            _publication_lease: publication_lease,
            ..
        } = self;
        drop(publication_lease);
        source.expect("managed runtime commit receipt always retains its source")
    }

    pub(crate) fn into_failure(self, cause: JavaRuntimeLookupError) -> ManagedRuntimeRebuildError {
        ManagedRuntimeRebuildError::Effect(ManagedRuntimeFailureReceipt {
            cache: self.cache,
            component: self.component,
            source: self.source.map(Box::new),
            cause,
            quarantine: self.quarantine,
            _publication_lease: self._publication_lease,
        })
    }
}

pub(crate) async fn finalize_managed_runtime_commit(
    receipt: ManagedRuntimeCommitReceipt,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt> {
    finalize_managed_runtime_commit_inner(receipt, PublishFailureMode::None).await
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn finalize_managed_runtime_commit_with_failure_for_test(
    receipt: ManagedRuntimeCommitReceipt,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt> {
    finalize_managed_runtime_commit_inner(receipt, PublishFailureMode::Finalization).await
}

#[cfg(test)]
pub(crate) async fn finalize_managed_runtime_commit_with_removed_quarantine_failure_for_test(
    receipt: ManagedRuntimeCommitReceipt,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt> {
    finalize_managed_runtime_commit_inner(receipt, PublishFailureMode::FinalizationAfterRemoval)
        .await
}

async fn finalize_managed_runtime_commit_inner(
    mut receipt: ManagedRuntimeCommitReceipt,
    failure_mode: PublishFailureMode,
) -> Result<ManagedRuntimeCommitReceipt, ManagedRuntimeFailureReceipt> {
    if !receipt.revalidate(&receipt.cache, &receipt.component).await {
        return Err(managed_runtime_finalization_failure(
            receipt,
            "managed runtime changed before whole-instance settlement",
        ));
    }
    let Some(quarantine) = receipt.quarantine.as_ref() else {
        return Ok(receipt);
    };
    let Some(install_root) = quarantine
        .cache
        .component_root(quarantine.component.as_str())
    else {
        return Err(managed_runtime_finalization_failure(
            receipt,
            "managed runtime quarantine escaped the managed cache",
        ));
    };
    let quarantine_root = runtime_sidecar_path(&install_root, "quarantine");
    if let Err(error) = finalize_runtime_quarantine(&quarantine_root, failure_mode).await {
        return Err(managed_runtime_finalization_failure(
            receipt,
            &format!("managed runtime quarantine finalization failed: {error}"),
        ));
    }
    receipt.quarantine = None;
    Ok(receipt)
}

fn managed_runtime_finalization_failure(
    receipt: ManagedRuntimeCommitReceipt,
    cause: &str,
) -> ManagedRuntimeFailureReceipt {
    let quarantine = receipt
        .quarantine
        .filter(|obligation| obligation.path_observation().retains_obligation());
    ManagedRuntimeFailureReceipt {
        cache: receipt.cache,
        component: receipt.component,
        source: receipt.source.map(Box::new),
        cause: JavaRuntimeLookupError::Install(cause.to_string()),
        quarantine,
        _publication_lease: receipt._publication_lease,
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
        let Some(install_root) = self.cache.component_root(self.component.as_str()) else {
            return RuntimePathObservation::Indeterminate;
        };
        observe_runtime_path(&runtime_sidecar_path(&install_root, "quarantine"))
    }
}

struct OwnedRuntimeStage {
    root: Option<PathBuf>,
}

impl OwnedRuntimeStage {
    fn new(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    fn root(&self) -> &Path {
        self.root
            .as_deref()
            .expect("owned runtime stage is present before promotion")
    }

    fn relinquish(&mut self) {
        self.root = None;
    }

    async fn cleanup(&mut self) -> std::io::Result<()> {
        let Some(root) = self.root.as_ref() else {
            return Ok(());
        };
        remove_runtime_sidecar(root).await?;
        self.root = None;
        Ok(())
    }
}

pub(crate) async fn stage_managed_runtime(
    cache: &ManagedRuntimeCache,
    component: &RuntimeId,
    source: RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<StagedManagedRuntime, JavaRuntimeLookupError> {
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
    let component_lock = cache.install_lock(component.as_str()).lock_owned().await;
    let file_lock = acquire_runtime_install_file_lock(&install_root).await?;
    let staging_root = runtime_sidecar_path(&install_root, "staging");
    remove_runtime_sidecar(&staging_root)
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    async_fs::create_dir(runtime_filesystem_path(&staging_root).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut stage = OwnedRuntimeStage::new(staging_root);
    let stage_result = materialize_runtime_tree_with_concurrency(
        component,
        stage.root(),
        &source,
        observer,
        download_concurrency,
        admission.download_bytes,
    )
    .await;
    if let Err(error) = stage_result {
        let _ = stage.cleanup().await;
        return Err(error);
    }
    if !runtime_tree_matches_source(stage.root(), &source).await {
        let _ = stage.cleanup().await;
        return Err(JavaRuntimeLookupError::Install(
            "staged runtime does not match its authenticated source".to_string(),
        ));
    }
    Ok(StagedManagedRuntime {
        cache: cache.clone(),
        component: component.clone(),
        install_root,
        stage,
        source: Some(source),
        publication_lease: Some(ManagedRuntimePublicationLease {
            _component_lock: component_lock,
            _file_lock: file_lock,
        }),
    })
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
    #[cfg(test)]
    FinalizationAfterRemoval,
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
    if expected_root != staged.install_root || staged.stage.root() != expected_stage {
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
    if source.component() != &staged.component
        || !runtime_tree_matches_source(staged.stage.root(), &source).await
    {
        let _ = staged.stage.cleanup().await;
        return Err(JavaRuntimeLookupError::Install(
            "staged runtime failed exact pre-promotion verification".to_string(),
        )
        .into());
    }

    let quarantine_root = runtime_sidecar_path(&staged.install_root, "quarantine");
    let mut canonical_exists = runtime_path_exists_async(&staged.install_root).await?;
    let mut quarantine_exists = runtime_path_exists_async(&quarantine_root).await?;
    let mut publication_effect_started = false;

    if canonical_exists && runtime_tree_matches_source(&staged.install_root, &source).await {
        staged
            .stage
            .cleanup()
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        return Ok(managed_runtime_commit_receipt(
            &mut staged,
            source,
            quarantine_exists,
        ));
    }

    if !canonical_exists && quarantine_exists {
        if let Err(error) = async_fs::rename(
            runtime_filesystem_path(&quarantine_root).as_ref(),
            runtime_filesystem_path(&staged.install_root).as_ref(),
        )
        .await
        {
            let _ = staged.stage.cleanup().await;
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(format!(
                    "managed runtime quarantine restoration failed: {error}"
                )),
            ));
        }
        publication_effect_started = true;
        canonical_exists = true;
        quarantine_exists = false;
    }

    if quarantine_exists {
        // Recursive removal can partially mutate the quarantine before reporting failure.
        publication_effect_started = true;
        let rotation_result = if failure_mode == PublishFailureMode::Rotation {
            Err(std::io::Error::other(
                "injected managed runtime quarantine rotation failure",
            ))
        } else {
            remove_runtime_sidecar(&quarantine_root).await
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

    let displaced_canonical = if canonical_exists {
        let displacement_result = if failure_mode == PublishFailureMode::Displacement {
            Err(std::io::Error::other(
                "injected managed runtime canonical displacement failure",
            ))
        } else {
            async_fs::rename(
                runtime_filesystem_path(&staged.install_root).as_ref(),
                runtime_filesystem_path(&quarantine_root).as_ref(),
            )
            .await
        };
        if let Err(error) = displacement_result {
            let _ = staged.stage.cleanup().await;
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(format!(
                    "managed runtime canonical displacement failed: {error}"
                )),
            ));
        }
        publication_effect_started = true;
        true
    } else {
        false
    };

    let promotion_result = if failure_mode == PublishFailureMode::Promotion {
        Err(std::io::Error::other(
            "injected managed runtime promotion failure",
        ))
    } else {
        async_fs::rename(
            runtime_filesystem_path(staged.stage.root()).as_ref(),
            runtime_filesystem_path(&staged.install_root).as_ref(),
        )
        .await
    };
    if let Err(promotion_error) = promotion_result {
        let restore_result = if displaced_canonical && inject_restoration_failure {
            Err(std::io::Error::other(
                "injected managed runtime restoration failure",
            ))
        } else if displaced_canonical {
            async_fs::rename(
                runtime_filesystem_path(&quarantine_root).as_ref(),
                runtime_filesystem_path(&staged.install_root).as_ref(),
            )
            .await
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
            JavaRuntimeLookupError::Install(promotion_error.to_string()),
        ));
    }
    publication_effect_started = true;

    if !runtime_tree_matches_source(&staged.install_root, &source).await {
        let failed_tree_result = async_fs::rename(
            runtime_filesystem_path(&staged.install_root).as_ref(),
            runtime_filesystem_path(staged.stage.root()).as_ref(),
        )
        .await;
        if failed_tree_result.is_err() {
            return Err(classify_managed_runtime_publish_failure(
                &mut staged,
                publication_effect_started,
                source,
                JavaRuntimeLookupError::Install(
                    "published runtime failed verification and could not be isolated".to_string(),
                ),
            ));
        }
        let restore_result = if displaced_canonical {
            async_fs::rename(
                runtime_filesystem_path(&quarantine_root).as_ref(),
                runtime_filesystem_path(&staged.install_root).as_ref(),
            )
            .await
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
        && let Err(error) = finalize_runtime_quarantine(&quarantine_root, failure_mode).await
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

async fn finalize_runtime_quarantine(
    quarantine_root: &Path,
    failure_mode: PublishFailureMode,
) -> std::io::Result<()> {
    #[cfg(test)]
    if failure_mode == PublishFailureMode::FinalizationAfterRemoval {
        remove_runtime_sidecar(quarantine_root).await?;
        return Err(std::io::Error::other(
            "injected failure after managed runtime quarantine removal",
        ));
    }
    if failure_mode == PublishFailureMode::Finalization {
        return Err(std::io::Error::other(
            "injected managed runtime quarantine finalization failure",
        ));
    }
    remove_runtime_sidecar(quarantine_root).await
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
    let quarantine_root = runtime_sidecar_path(&staged.install_root, "quarantine");
    ManagedRuntimeRebuildError::Effect(ManagedRuntimeFailureReceipt {
        cache: staged.cache.clone(),
        component: staged.component.clone(),
        source: Some(Box::new(source)),
        cause,
        quarantine: observe_runtime_path(&quarantine_root)
            .retains_obligation()
            .then(|| ManagedRuntimeQuarantineObligation {
                cache: staged.cache.clone(),
                component: staged.component.clone(),
            }),
        _publication_lease: staged
            .publication_lease
            .take()
            .expect("managed runtime publication retains its lease until terminal settlement"),
    })
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

pub(super) async fn runtime_tree_matches_source(
    root: &Path,
    source: &RuntimeSourceReceipt,
) -> bool {
    let Ok(root_metadata) =
        async_fs::symlink_metadata(runtime_filesystem_path(root).as_ref()).await
    else {
        return false;
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return false;
    }
    let Ok(expected_proof) = component_manifest_proof_bytes(source.manifest()) else {
        return false;
    };
    let proof_path = root.join(COMPONENT_MANIFEST_PROOF_FILE);
    if !runtime_regular_file_matches(&proof_path, &expected_proof).await
        || !runtime_regular_file_matches(&root.join(".axial-ready"), b"ready").await
    {
        return false;
    }
    let root = root.to_path_buf();
    let component = source.component().clone();
    let source_manifest = source.manifest().clone();
    tokio::task::spawn_blocking(move || {
        super::discovery::managed_runtime_contents_verified_for_component(&root, &component)
            && runtime_tree_shape_matches_manifest(&component, &root, &source_manifest)
    })
    .await
    .unwrap_or(false)
}

async fn runtime_regular_file_matches(path: &Path, expected: &[u8]) -> bool {
    let Ok(metadata) = async_fs::symlink_metadata(runtime_filesystem_path(path).as_ref()).await
    else {
        return false;
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != expected.len() as u64
    {
        return false;
    }
    async_fs::read(runtime_filesystem_path(path).as_ref())
        .await
        .is_ok_and(|actual| actual == expected)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RuntimeTreeNodeKind {
    Directory,
    File,
    Link,
}

fn runtime_tree_shape_matches_manifest(
    component: &RuntimeId,
    root: &Path,
    manifest: &ComponentManifest,
) -> bool {
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
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
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
    prefixes: &mut HashMap<String, String>,
    canonical_path: &Path,
    filesystem_key: &str,
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
    prefixes: &HashMap<String, String>,
    canonical_path: &Path,
    filesystem_key: &str,
) -> bool {
    runtime_manifest_path_prefixes(canonical_path, filesystem_key).is_some_and(|path_prefixes| {
        path_prefixes
            .into_iter()
            .all(|(folded, canonical)| prefixes.get(&folded) == Some(&canonical))
    })
}

fn runtime_manifest_path_prefixes(
    canonical_path: &Path,
    filesystem_key: &str,
) -> Option<Vec<(String, String)>> {
    let canonical_segments = canonical_path
        .iter()
        .map(|segment| segment.to_str())
        .collect::<Option<Vec<_>>>();
    let canonical_segments = canonical_segments?;
    let folded_segments = filesystem_key.split('/').collect::<Vec<_>>();
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
        prefixes.push((folded_prefix.clone(), canonical_prefix.clone()));
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

pub(crate) fn runtime_source_matches_known_good_inventory(
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

fn observe_runtime_path(path: &Path) -> RuntimePathObservation {
    match std::fs::symlink_metadata(runtime_filesystem_path(path).as_ref()) {
        Ok(_) => RuntimePathObservation::Present,
        Err(error) => runtime_path_error_observation(&error),
    }
}

fn runtime_path_error_observation(error: &std::io::Error) -> RuntimePathObservation {
    if error.kind() == std::io::ErrorKind::NotFound {
        RuntimePathObservation::Absent
    } else {
        RuntimePathObservation::Indeterminate
    }
}

async fn runtime_path_exists_async(path: &Path) -> Result<bool, JavaRuntimeLookupError> {
    match async_fs::symlink_metadata(runtime_filesystem_path(path).as_ref()).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(JavaRuntimeLookupError::Install(error.to_string())),
    }
}

async fn remove_runtime_sidecar(path: &Path) -> std::io::Result<()> {
    let metadata = match async_fs::symlink_metadata(runtime_filesystem_path(path).as_ref()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        async_fs::remove_dir_all(runtime_filesystem_path(path).as_ref()).await
    } else {
        async_fs::remove_file(runtime_filesystem_path(path).as_ref()).await
    }
}

struct RuntimeInstallFileLock {
    file: std::fs::File,
}

impl Drop for RuntimeInstallFileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

async fn acquire_runtime_install_file_lock(
    install_root: &Path,
) -> Result<RuntimeInstallFileLock, JavaRuntimeLookupError> {
    let lock_path = runtime_install_lock_file_path(install_root);
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(runtime_filesystem_path(parent).as_ref())?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(runtime_filesystem_path(&lock_path).as_ref())?;
        file.lock()?;
        Ok::<_, std::io::Error>(RuntimeInstallFileLock { file })
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

pub(super) fn runtime_install_lock_file_path(install_root: &Path) -> PathBuf {
    runtime_sidecar_path(install_root, "install.lock")
}

#[cfg(test)]
impl StagedManagedRuntime {
    pub(super) fn staging_root_for_test(&self) -> &Path {
        self.stage.root()
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
    dest_dir: &Path,
    source: &RuntimeSourceReceipt,
    max_entries: usize,
    max_bytes: u64,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    let admission = validate_ephemeral_processor_manifest(source, max_entries, max_bytes)?;
    if async_fs::symlink_metadata(runtime_filesystem_path(dest_dir).as_ref())
        .await
        .is_ok()
    {
        return Err(JavaRuntimeLookupError::Install(
            "processor runtime destination already exists".to_string(),
        ));
    }
    async_fs::create_dir_all(runtime_filesystem_path(dest_dir).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let result = materialize_runtime_tree_with_concurrency(
        component,
        dest_dir,
        source,
        observer,
        1,
        admission.download_bytes,
    )
    .await;
    if result.is_err() {
        let _ = remove_runtime_sidecar(dest_dir).await;
    }
    result
}

async fn materialize_runtime_tree_with_concurrency(
    component: &RuntimeId,
    dest_dir: &Path,
    source: &RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
    admitted_download_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    if source.component() != component {
        return Err(JavaRuntimeLookupError::Install(
            "runtime source component mismatch".to_string(),
        ));
    }
    let metadata = async_fs::symlink_metadata(runtime_filesystem_path(dest_dir).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JavaRuntimeLookupError::Install(
            "runtime materialization destination is not an owned directory".to_string(),
        ));
    }
    let install_result = async {
        let component_manifest = source.manifest();
        persist_component_manifest_proof(dest_dir, component_manifest).await?;

        install_runtime_manifest_files_with_concurrency(
            component,
            dest_dir,
            component_manifest.files.clone(),
            observer,
            download_concurrency,
            admitted_download_bytes,
        )
        .await?;

        let java_exe = java_executable(dest_dir);
        if !runtime_executable_ready(&java_exe) {
            return Err(JavaRuntimeLookupError::Install(format!(
                "installed runtime {} is incomplete",
                component.as_str()
            )));
        }

        Ok(())
    }
    .await;

    install_result?;

    let ready_marker = dest_dir.join(".axial-ready");
    if let Err(error) =
        async_fs::write(runtime_filesystem_path(&ready_marker).as_ref(), b"ready").await
    {
        return Err(JavaRuntimeLookupError::Install(error.to_string()));
    }

    Ok(())
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
        COMPONENT_MANIFEST_PROOF_FILE.to_ascii_lowercase(),
        ".axial-ready".to_string(),
    ]);
    let mut filesystem_entries = HashMap::new();
    let mut collision_entries = HashMap::new();
    let mut link_targets = Vec::new();
    for reserved_path in &reserved_paths {
        let inserted = insert_runtime_tree_node(
            &mut collision_entries,
            PathBuf::from(reserved_path),
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
                link_targets.push(resolved_target);
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
                    transient_paths.push(format!("{filesystem_key}.axial-tmp.lzma"));
                    compressed_size
                } else {
                    raw_size
                };
                transient_paths.push(format!("{filesystem_key}.axial-tmp"));
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
            PathBuf::from(&filesystem_key),
            kind,
        ) {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                "runtime manifest contains an invalid path topology",
            ));
        }
        if !insert_runtime_tree_node(&mut collision_entries, PathBuf::from(&filesystem_key), kind) {
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
                    PathBuf::from(transient_path),
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
    for target in link_targets {
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
        let target_kind = filesystem_entries.get(&PathBuf::from(&target_key));
        if !runtime_manifest_prefix_spellings_match(
            &canonical_prefixes,
            &canonical_target,
            &target_key,
        ) || !target_kind.is_some_and(|kind| *kind != RuntimeTreeNodeKind::Link)
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

async fn persist_component_manifest_proof(
    temp_dir: &Path,
    component_manifest: &ComponentManifest,
) -> Result<(), JavaRuntimeLookupError> {
    let bytes = component_manifest_proof_bytes(component_manifest)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let proof_path = temp_dir.join(COMPONENT_MANIFEST_PROOF_FILE);
    async_fs::write(runtime_filesystem_path(&proof_path).as_ref(), bytes)
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

#[cfg(test)]
pub(super) async fn install_runtime_manifest_files(
    component: &RuntimeId,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
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
    install_runtime_manifest_files_with_concurrency(
        component,
        temp_dir,
        files,
        observer,
        runtime_file_download_concurrency(),
        admitted_download_bytes,
    )
    .await
}

async fn install_runtime_manifest_files_with_concurrency(
    component: &RuntimeId,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
    admitted_download_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    let plan = plan_runtime_manifest_files(files);
    let download_client = runtime_download_client();

    for (relative_path, file) in plan.directory_entries.into_iter().chain(plan.other_entries) {
        install_runtime_manifest_file(
            component,
            download_client.clone(),
            temp_dir,
            &relative_path,
            file,
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

    let mut file_downloads =
        futures_util::stream::iter(plan.file_entries.into_iter().map(|entry| {
            let download_client = download_client.clone();
            let temp_dir = temp_dir.to_path_buf();
            let component = component.clone();
            async move {
                let (relative_path, file) = entry;
                let bytes = runtime_manifest_file_download_bytes(&component, &file)?;
                Box::pin(install_runtime_manifest_file(
                    &component,
                    download_client,
                    &temp_dir,
                    &relative_path,
                    file,
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
    while let Some(result) = file_downloads.next().await {
        let completed = result?;
        completed_files += 1;
        completed_bytes = completed_bytes
            .checked_add(completed.bytes)
            .ok_or_else(|| {
                JavaRuntimeLookupError::Install(
                    "runtime download progress byte total overflowed".to_string(),
                )
            })?;
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.as_str().to_string(),
            current: completed_files,
            total: total_files,
            bytes_done: completed_bytes,
            bytes_total: total_bytes,
        });
    }

    for (relative_path, file) in plan.link_entries {
        install_runtime_manifest_file(
            component,
            download_client.clone(),
            temp_dir,
            &relative_path,
            file,
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

pub(super) async fn install_runtime_manifest_file(
    component: &RuntimeId,
    download_client: reqwest::Client,
    temp_dir: &Path,
    relative_path: &str,
    file: ComponentManifestFile,
) -> Result<(), JavaRuntimeLookupError> {
    let destination = component_manifest_destination(component, temp_dir, relative_path)?;
    if file.kind == "directory" {
        async_fs::create_dir_all(runtime_filesystem_path(&destination).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        return Ok(());
    }
    if file.kind == "link" {
        return install_runtime_manifest_link(
            component,
            temp_dir,
            &destination,
            relative_path,
            &file,
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

    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(runtime_filesystem_path(parent).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }

    let temp_path = runtime_download_temp_path(&destination);
    if let Some(lzma) = lzma {
        Box::pin(fetch_lzma_runtime_file(
            component,
            &download_client,
            &lzma,
            &raw,
            &temp_path,
            relative_path,
        ))
        .await?;
    } else {
        let expected = RuntimeDownloadEvidence::from(&raw);
        Box::pin(fetch_runtime_file(
            component,
            &download_client,
            &raw.url,
            &temp_path,
            expected,
            relative_path,
        ))
        .await?;
    }
    if let Err(error) = async_fs::rename(
        runtime_filesystem_path(&temp_path).as_ref(),
        runtime_filesystem_path(&destination).as_ref(),
    )
    .await
    {
        let _ = async_fs::remove_file(runtime_filesystem_path(&temp_path).as_ref()).await;
        return Err(JavaRuntimeLookupError::Install(error.to_string()));
    }
    #[cfg(unix)]
    if file.executable {
        use std::os::unix::fs::PermissionsExt;

        let permissions = std::fs::Permissions::from_mode(0o755);
        async_fs::set_permissions(runtime_filesystem_path(&destination).as_ref(), permissions)
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
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

async fn fetch_lzma_runtime_file(
    component: &RuntimeId,
    download_client: &reqwest::Client,
    lzma: &ComponentManifestDownload,
    raw: &ComponentManifestDownload,
    temp_path: &Path,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let lzma_temp_path = runtime_lzma_download_temp_path(temp_path);
    let compressed_expected = RuntimeDownloadEvidence::from(lzma);
    let raw_expected = RuntimeDownloadEvidence::from(raw);
    let result = async {
        Box::pin(fetch_runtime_file(
            component,
            download_client,
            &lzma.url,
            &lzma_temp_path,
            compressed_expected,
            relative_path,
        ))
        .await?;
        decompress_lzma_runtime_file_to_temp(
            component.clone(),
            &lzma_temp_path,
            temp_path,
            raw_expected,
            relative_path.to_string(),
        )
        .await
    }
    .await;

    let _ = async_fs::remove_file(runtime_filesystem_path(&lzma_temp_path).as_ref()).await;
    if result.is_err() {
        let _ = async_fs::remove_file(runtime_filesystem_path(temp_path).as_ref()).await;
    }
    result
}

fn runtime_lzma_download_temp_path(temp_path: &Path) -> std::path::PathBuf {
    let mut name = temp_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("runtime-download"))
        .to_os_string();
    name.push(".lzma");
    temp_path.with_file_name(name)
}

async fn decompress_lzma_runtime_file_to_temp(
    component: RuntimeId,
    compressed_path: &Path,
    output_path: &Path,
    expected: RuntimeDownloadEvidence,
    relative_path: String,
) -> Result<(), JavaRuntimeLookupError> {
    let compressed_path = compressed_path.to_path_buf();
    let output_path = output_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let input = std::fs::File::open(runtime_filesystem_path(&compressed_path).as_ref())
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        let output = std::fs::File::create(runtime_filesystem_path(&output_path).as_ref())
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        let mut input = RuntimeInstallReader::new(BufReader::new(input));
        let mut output = RuntimeIntegrityWriter::new(
            output,
            component.clone(),
            expected.clone(),
            &relative_path,
        );
        decompress_lzma_stream(&component, &mut input, &mut output)?;
        output
            .flush()
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        let actual = output.actual();
        verify_runtime_download(&relative_path, &expected, &actual).map_err(|error| {
            runtime_source_failure(
                &component,
                RuntimeSourceFailureKind::IntegrityMismatch,
                error.to_string(),
            )
        })
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
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
}

impl<R> RuntimeInstallReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            failure: None,
        }
    }

    fn take_failure(&mut self) -> Option<JavaRuntimeLookupError> {
        self.failure.take().map(JavaRuntimeLookupError::Install)
    }
}

impl<R: Read> Read for RuntimeInstallReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer).inspect_err(|error| {
            self.failure.get_or_insert_with(|| error.to_string());
        })
    }
}

impl<R: BufRead> BufRead for RuntimeInstallReader<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
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
}

impl<W> RuntimeIntegrityWriter<W> {
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
        }
    }

    fn take_failure(&mut self) -> Option<JavaRuntimeLookupError> {
        self.failure.take()
    }

    fn actual(self) -> RuntimeDownloadActual {
        RuntimeDownloadActual {
            size: self.size,
            sha1: format!("{:x}", self.hasher.finalize()),
        }
    }
}

impl<W: Write> Write for RuntimeIntegrityWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
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
        self.output.flush().inspect_err(|error| {
            self.failure = Some(JavaRuntimeLookupError::Install(error.to_string()));
        })
    }
}

async fn install_runtime_manifest_link(
    component: &RuntimeId,
    temp_dir: &Path,
    destination: &Path,
    relative_path: &str,
    file: &ComponentManifestFile,
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
    component_manifest_link_target_path(component, temp_dir, destination, relative_path, target)?;
    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(runtime_filesystem_path(parent).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    }

    install_runtime_manifest_symlink(target.to_string(), destination.to_path_buf()).await
}

#[cfg(unix)]
async fn install_runtime_manifest_symlink(
    target: String,
    destination: std::path::PathBuf,
) -> Result<(), JavaRuntimeLookupError> {
    tokio::task::spawn_blocking(move || {
        std::os::unix::fs::symlink(target, runtime_filesystem_path(&destination).as_ref())
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

#[cfg(not(unix))]
async fn install_runtime_manifest_symlink(
    _target: String,
    _destination: std::path::PathBuf,
) -> Result<(), JavaRuntimeLookupError> {
    Err(JavaRuntimeLookupError::Install(
        "runtime manifest link entries are unsupported on this platform".to_string(),
    ))
}

fn runtime_sha1_is_valid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
        RuntimePathObservation, runtime_path_error_observation,
    };
    use crate::runtime::{ManagedRuntimeCache, RuntimeId};

    #[test]
    fn quarantine_obligation_is_omitted_only_for_not_found() {
        let absent =
            runtime_path_error_observation(&std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(absent, RuntimePathObservation::Absent);
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
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Other,
        ] {
            let indeterminate = runtime_path_error_observation(&std::io::Error::from(kind));
            assert_eq!(indeterminate, RuntimePathObservation::Indeterminate);
            assert_eq!(
                ManagedRuntimeQuarantineObservation::from(indeterminate),
                ManagedRuntimeQuarantineObservation::Indeterminate
            );
            assert!(!indeterminate.is_present());
            assert!(indeterminate.retains_obligation());
        }
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
