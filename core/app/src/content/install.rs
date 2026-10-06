//! Content batches bind exact byte preconditions to a registered instance.
//!
//! The SQLite receipt is durable before the first payload effect. Each native
//! replacement uses the retained streamed content transaction. The content receipt is
//! removed only after all payloads and the final manifest have been verified.
//! A pending receipt fences ordinary instance admission across process restart.

use super::{
    catalog::{ContentError, ContentResult, ContentService},
    model::{CanonicalId, ContentDependency, ContentKind, FileRef, ProviderId},
    packs::{PackFileSelection, PackPlan, ResolvedPack, validate_download_url},
    provenance::{
        ContentManifest, LiveManagedContent, MANIFEST_FILE, MAX_MANIFEST_BYTES,
        ManagedContentFileName, ManifestEntry, PackInstallation, PackInstalledFile,
    },
    resolve::{ContentPlanState, ResolutionSelection, TargetedPlan},
};
use crate::{
    files::{PortableName, ScopedDirectory, ScopedPath},
    instances::{
        directory::{InstanceDirectories, RegisteredInstance},
        model::InstanceId,
    },
    network::ProviderClient,
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{self, OptionalExtension, params},
    },
    tasks::{CancellationToken, TaskHandle, TaskOwner},
};
use axial_minecraft::{
    DownloadProgress,
    download::{
        ExpectedTransferDigests, ManagedTransferAuthority, TransferContract, TransferFailureKind,
        TransferOrigin, transfer_cancellation_channel,
    },
    managed_path::*,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    sync::{Arc, Mutex},
};

pub const MAX_CONTENT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BATCH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RECEIPT_BYTES: usize = 16 * 1024 * 1024;

pub const MIGRATION: Migration = Migration {
    id: "content-batches.v1",
    sql: "CREATE TABLE content_batches (
        instance_id TEXT PRIMARY KEY NOT NULL,
        operation_id TEXT NOT NULL UNIQUE,
        receipt_json TEXT NOT NULL CHECK(length(receipt_json) <= 16777216)
    ) STRICT;",
};

pub fn has_pending(storage: &MetadataStore, id: &InstanceId) -> Result<bool, StorageError> {
    storage.read(|connection| {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM content_batches WHERE instance_id = ?1)",
            [id.as_str()],
            |row| row.get(0),
        )?)
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedFile {
    pub canonical_id: CanonicalId,
    pub provider: ProviderId,
    pub project_id: String,
    pub version_id: String,
    pub kind: ContentKind,
    pub file: FileRef,
    pub dependencies: Vec<ContentDependency>,
    pub title: Option<String>,
}

impl PlannedFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        canonical_id: CanonicalId,
        provider: ProviderId,
        project_id: String,
        version_id: String,
        kind: ContentKind,
        file: FileRef,
        dependencies: Vec<ContentDependency>,
        title: Option<String>,
    ) -> ContentResult<Self> {
        validate_planned_artifact(kind, &file)?;
        ManifestEntry::managed(
            canonical_id.clone(),
            provider,
            project_id.clone(),
            version_id.clone(),
            kind,
            &file,
            dependencies.clone(),
            title.clone(),
        )?;
        Ok(Self {
            canonical_id,
            provider,
            project_id,
            version_id,
            kind,
            file,
            dependencies,
            title,
        })
    }

    fn entry(&self) -> ContentResult<ManifestEntry> {
        ManifestEntry::managed(
            self.canonical_id.clone(),
            self.provider,
            self.project_id.clone(),
            self.version_id.clone(),
            self.kind,
            &self.file,
            self.dependencies.clone(),
            self.title.clone(),
        )
    }
}

pub fn validate_planned_artifact(
    kind: ContentKind,
    file: &FileRef,
) -> ContentResult<(u64, ManagedContentFileName)> {
    let invalid = || {
        ContentError::ProviderMetadataInvalid(
            "content artifact has invalid path, size, or integrity metadata".into(),
        )
    };
    if kind.install_subdir().is_none() {
        return Err(invalid());
    }
    let name = ManagedContentFileName::new_exact(&file.filename).map_err(|_| invalid())?;
    if kind == ContentKind::Mod && !name.key().as_str().ends_with(".jar") {
        return Err(invalid());
    }
    let size = file
        .size
        .filter(|size| (1..=MAX_CONTENT_FILE_BYTES).contains(size))
        .ok_or_else(invalid)?;
    if !file
        .sha512
        .as_deref()
        .is_some_and(super::provenance::valid_sha512)
        || file
            .sha1
            .as_ref()
            .is_some_and(|hash| hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid());
    }
    let admitted_url = validate_download_url(&file.url).is_ok();
    #[cfg(any(test, feature = "test-support"))]
    let admitted_url = admitted_url
        || reqwest::Url::parse(&file.url).is_ok_and(|url| {
            url.fragment().is_none()
                && TransferOrigin::from_loopback_http_for_test_support(&url).is_ok()
        });
    if !admitted_url {
        return Err(invalid());
    }
    Ok((size, name))
}

#[derive(Debug, thiserror::Error)]
pub enum MutationError {
    #[error("instance content changed; review a fresh plan")]
    Changed,
    #[error("content files conflict with existing user files")]
    Conflict,
    #[error("content operation requires settlement before this instance can be used")]
    Pending,
    #[error("content operation was cancelled before publication")]
    Cancelled,
    #[error("content exceeds the supported size limit")]
    Capacity,
    #[error("content is still required by another installed project")]
    Required,
    #[error("content not found")]
    NotFound,
    #[error("managed mods must be removed through content operations")]
    Managed,
    #[error("content is unavailable")]
    Unavailable,
    #[error("content payload failed integrity verification")]
    Integrity,
    #[error("content files could not be updated")]
    Files,
    #[error("safe streaming content publication is unavailable on this filesystem")]
    StreamingUnsupported,
    #[error("content metadata could not be persisted")]
    Storage(#[from] StorageError),
    #[error("content request could not be completed")]
    Content(#[from] ContentError),
    #[error(transparent)]
    Pack(#[from] super::packs::PackError),
}

impl From<rusqlite::Error> for MutationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl MutationError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Changed | Self::Conflict | Self::Pending | Self::Required | Self::Managed => 409,
            Self::NotFound => 404,
            Self::Cancelled => 409,
            Self::Capacity => 413,
            Self::Unavailable => 503,
            Self::Integrity => 422,
            Self::StreamingUnsupported => 501,
            Self::Pack(super::packs::PackError::TooLarge) => 413,
            Self::Pack(
                super::packs::PackError::Cancelled
                | super::packs::PackError::SelectionChanged
                | super::packs::PackError::Conflict(_),
            ) => 409,
            Self::Pack(super::packs::PackError::Integrity) => 422,
            Self::Pack(super::packs::PackError::Download | super::packs::PackError::Provider) => {
                503
            }
            Self::Pack(_) | Self::Content(_) => 400,
            Self::Files | Self::Storage(_) => 500,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    size: u64,
    sha512: String,
}

impl Proof {
    fn bytes(bytes: &[u8]) -> Self {
        Self {
            size: bytes.len() as u64,
            sha512: format!("{:x}", Sha512::digest(bytes)),
        }
    }
    fn entry(entry: &ManifestEntry) -> Result<Self, MutationError> {
        Ok(Self {
            size: entry.size().ok_or(MutationError::Changed)?,
            sha512: entry.sha512().ok_or(MutationError::Changed)?.into(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    path: String,
    before: Option<Proof>,
    after: Option<Proof>,
    /// Downloads are provider-authored, validated again on restart. A local
    /// copy is an exact source selector below the same registered directory.
    source: Option<Source>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Download { file: FileRef },
    Local { path: String, proof: Proof },
    PackDownload { file: FileRef },
    PackOverride,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u32,
    operation_id: String,
    instance_id: InstanceId,
    instance_revision: u64,
    directory_receipt: String,
    before_manifest: Option<Vec<u8>>,
    before_observed_manifest: Vec<u8>,
    after_manifest: Vec<u8>,
    changes: Vec<Change>,
    native_settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack: Option<CanonicalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_mod: Option<LocalModIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ready_checkpoint: Option<String>,
}

fn encoded_receipt(receipt: &Receipt) -> Result<String, MutationError> {
    let encoded = serde_json::to_string(receipt).map_err(|_| MutationError::Capacity)?;
    if encoded.len() > MAX_RECEIPT_BYTES {
        return Err(MutationError::Capacity);
    }
    Ok(encoded)
}

fn checkpoint_binding(receipt: &Receipt) -> Result<[u8; 32], MutationError> {
    let mut intent = receipt.clone();
    intent.native_settled = false;
    intent.ready_checkpoint = None;
    let mut digest = Sha256::new();
    digest.update(b"axial.content.ready-checkpoint.v1\0");
    digest.update(encoded_receipt(&intent)?.as_bytes());
    Ok(digest.finalize().into())
}

fn checkpoint_budget(receipt: &Receipt) -> Result<usize, MutationError> {
    let mut empty = receipt.clone();
    empty.ready_checkpoint = Some(String::new());
    // Nested checkpoint JSON can double in size when encoded as a receipt string.
    Ok(MAX_RECEIPT_BYTES.saturating_sub(encoded_receipt(&empty)?.len()) / 2)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalModIntent {
    source: String,
    destination: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MutationReceipt {
    pub operation_id: String,
    pub instance_id: InstanceId,
    pub status: &'static str,
    pub changed_files: usize,
}

/// Retains exact native effects and instance/library exclusion. No receipt is
/// converted to a path or acknowledged merely because a request disconnected.
struct PendingBatch {
    instance: RegisteredInstance,
    effects: Vec<Effect>,
    restart_blocked: bool,
}

enum Effect {
    Recovery(ManagedContentRecovery),
    Committed,
    RolledBack,
}

impl Effect {
    fn settle(self, owner: &ContentMutations, receipt: &mut Receipt) -> Self {
        match self {
            Self::Committed => Self::Committed,
            Self::RolledBack => Self::RolledBack,
            Self::Recovery(recovery) => match owner.reconcile_recovery(receipt, recovery) {
                ManagedContentTransactionOutcome::RecoveryRequired(effect) => {
                    Self::Recovery(effect)
                }
                ManagedContentTransactionOutcome::Committed(_) => Self::Committed,
                ManagedContentTransactionOutcome::Cancelled(_)
                | ManagedContentTransactionOutcome::Failed(_) => Self::RolledBack,
            },
        }
    }
}

#[derive(Clone)]
pub struct ContentMutations {
    directories: InstanceDirectories,
    _client: ProviderClient,
    tasks: TaskOwner,
    pending: Arc<Mutex<HashMap<InstanceId, PendingBatch>>>,
    resuming: Arc<Mutex<HashSet<InstanceId>>>,
    performance: Option<crate::performance::mutation::PerformanceService>,
    progress: Option<Arc<dyn Fn(DownloadProgress) + Send + Sync>>,
    #[cfg(test)]
    transfer_fixture: Option<(reqwest::Url, axial_minecraft::download::TransferClient)>,
}

impl ContentMutations {
    pub fn new(directories: InstanceDirectories, client: ProviderClient, tasks: TaskOwner) -> Self {
        Self {
            directories,
            _client: client,
            tasks,
            pending: Arc::new(Mutex::new(HashMap::new())),
            resuming: Arc::new(Mutex::new(HashSet::new())),
            performance: None,
            progress: None,
            #[cfg(test)]
            transfer_fixture: None,
        }
    }

    pub fn with_performance(
        mut self,
        performance: crate::performance::mutation::PerformanceService,
    ) -> Self {
        self.performance = Some(performance);
        self
    }

    /// Attach to an operation-local clone; the queue owns observable lifecycle.
    pub(crate) fn with_progress(
        mut self,
        callback: Arc<dyn Fn(DownloadProgress) + Send + Sync>,
    ) -> Self {
        self.progress = Some(callback);
        self
    }

    pub(crate) async fn protect_performance_mod(
        &self,
        instance: &RegisteredInstance,
        filename: &str,
    ) -> Result<(), MutationError> {
        let service = self
            .performance
            .as_ref()
            .ok_or(MutationError::Unavailable)?;
        let proofs = service
            .managed_witness_proofs(instance)
            .await
            .map_err(|_| MutationError::Unavailable)?;
        if proofs.iter().any(|proof| proof.protects_filename(filename)) {
            return Err(MutationError::Conflict);
        }
        Ok(())
    }

    pub fn directories(&self) -> &InstanceDirectories {
        &self.directories
    }

    pub async fn resolve_pack(
        &self,
        service: &ContentService,
        canonical_id: &CanonicalId,
        version_id: Option<&str>,
    ) -> Result<super::packs::ResolvedPack, MutationError> {
        Ok(super::packs::resolve_pack(
            service,
            &self._client,
            canonical_id,
            version_id,
            &CancellationToken::new(),
        )
        .await?)
    }

    pub fn install_pack(
        &self,
        service: &ContentService,
        id: &InstanceId,
        pack: ResolvedPack,
        include_overrides: bool,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        let plan = pack.archive.plan_all(include_overrides)?;
        let service = service.clone();
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                owner
                    .install_pack_admitted(
                        &service,
                        instance,
                        pack.canonical_id,
                        pack.version_id,
                        plan,
                        &cancel,
                    )
                    .await
            })
            .map_err(|_| MutationError::Unavailable)
    }

    /// Check a completed creation handoff without downloading the archive
    /// again. Old provenance without authenticated destinations cannot pass.
    pub(crate) fn installed_pack_admitted(
        &self,
        instance: &RegisteredInstance,
        canonical_id: &CanonicalId,
        version_id: &str,
        fingerprint: &str,
    ) -> Result<bool, MutationError> {
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        let (manifest, _, _) = observe(instance.game_directory())?;
        let Some(entry) = manifest.find(canonical_id) else {
            return Ok(false);
        };
        let Some(proof) = entry.pack_installation() else {
            return Ok(false);
        };
        if entry.kind() != ContentKind::Modpack
            || entry.version_id() != version_id
            || proof.fingerprint != fingerprint
        {
            return Ok(false);
        }
        for file in &proof.files {
            if observe_path(instance.game_directory(), &file.path)?
                != Some(Proof {
                    size: file.size,
                    sha512: file.sha512.clone(),
                })
            {
                return Err(MutationError::Changed);
            }
        }
        Ok(true)
    }

    /// The caller retains instance/setup admission. New destinations must be
    /// absent; renamed known members retire only exact previously owned bytes.
    pub(crate) async fn install_pack_admitted(
        &self,
        service: &ContentService,
        instance: RegisteredInstance,
        canonical_id: CanonicalId,
        version_id: String,
        plan: PackPlan,
        cancel: &CancellationToken,
    ) -> Result<MutationReceipt, MutationError> {
        let target = &instance.record().instance;
        if cancel.is_cancelled() {
            return Err(MutationError::Cancelled);
        }
        if self.installed_pack_admitted(
            &instance,
            &canonical_id,
            &version_id,
            plan.fingerprint(),
        )? {
            let (manifest, _, _) = observe(instance.game_directory())?;
            let installed = manifest
                .find(&canonical_id)
                .and_then(ManifestEntry::pack_installation)
                .ok_or(MutationError::Changed)?;
            let destinations = plan.destinations().into_iter().collect::<HashSet<_>>();
            if installed
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<HashSet<_>>()
                == destinations
            {
                return Ok(MutationReceipt {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    instance_id: target.id.clone(),
                    status: "complete",
                    changed_files: 0,
                });
            }
            return Err(MutationError::Conflict);
        }
        let (before, _, raw) = observe(instance.game_directory())?;
        for path in plan.destinations() {
            if observe_path(instance.game_directory(), path)?.is_some() {
                return Err(MutationError::Conflict);
            }
            if let Some(name) = path
                .strip_prefix("mods/")
                .filter(|name| !name.contains('/'))
            {
                self.protect_performance_mod(&instance, name).await?;
            }
        }
        let mut changes = Vec::new();
        for file in plan.files() {
            if cancel.is_cancelled() {
                return Err(MutationError::Cancelled);
            }
            let authenticated = file.authenticated_file(&self._client, cancel).await?;
            changes.push(Change {
                path: file.path.clone(),
                before: None,
                after: Some(Proof {
                    size: authenticated.size.ok_or(MutationError::Integrity)?,
                    sha512: authenticated
                        .sha512
                        .clone()
                        .ok_or(MutationError::Integrity)?,
                }),
                source: Some(Source::PackDownload {
                    file: authenticated,
                }),
            });
        }
        for (path, size, sha512) in plan.overrides() {
            changes.push(Change {
                path: path.into(),
                before: None,
                after: Some(Proof {
                    size,
                    sha512: sha512.into(),
                }),
                source: Some(Source::PackOverride),
            });
        }
        let mut entry = ManifestEntry::provenance(
            canonical_id.clone(),
            ProviderId::Modrinth,
            canonical_id.project_id().into(),
            version_id,
            Some(plan.index().name.clone()),
        )?;
        entry.record_pack_installation(PackInstallation {
            fingerprint: plan.fingerprint().into(),
            files: changes
                .iter()
                .map(|change| {
                    let proof = change
                        .after
                        .as_ref()
                        .expect("pack changes install exact bytes");
                    PackInstalledFile {
                        path: change.path.clone(),
                        size: proof.size,
                        sha512: proof.sha512.clone(),
                    }
                })
                .collect(),
        })?;
        let mut after = before.clone();
        after.try_upsert(entry)?;
        let members =
            identify_pack_members(service, &plan, &changes, &canonical_id, cancel).await?;
        after.try_upsert_batch(members)?;
        for (path, proof) in pack_stale_paths(&before, &after, &canonical_id)? {
            if let Some(name) = path.strip_prefix("mods/") {
                self.protect_performance_mod(&instance, name).await?;
            }
            match observe_path(instance.game_directory(), &path)? {
                Some(current) if current == proof => changes.push(Change {
                    path,
                    before: Some(proof),
                    after: None,
                    source: None,
                }),
                Some(_) => return Err(MutationError::Conflict),
                None => {}
            }
        }
        let receipt = Receipt {
            schema: 1,
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: target.id.clone(),
            instance_revision: instance.record().revision,
            directory_receipt: instance
                .game_directory()
                .receipt()
                .map_err(|_| MutationError::Changed)?,
            before_manifest: raw,
            before_observed_manifest: before.encode_managed()?,
            after_manifest: after.encode_managed()?,
            changes,
            native_settled: false,
            pack: Some(canonical_id),
            local_mod: None,
            ready_checkpoint: None,
        };
        validate_receipt(&receipt)?;
        if cancel.is_cancelled() {
            return Err(MutationError::Cancelled);
        }
        validate_instance(&instance, &receipt)?;
        preflight(instance.game_directory(), &receipt, false)?;
        self.persist_receipt(&receipt)?;
        self.apply_pack(instance, receipt, HashMap::new(), Some(&plan), cancel)
            .await
    }

    pub async fn pack_files(
        &self,
        service: &ContentService,
        id: &InstanceId,
        canonical_id: &CanonicalId,
        version_id: Option<&str>,
    ) -> Result<super::packs::ModpackFilesPlan, MutationError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        Ok(self
            .pack_preview(service, &instance, canonical_id, version_id)
            .await?
            .snapshot())
    }

    async fn pack_preview(
        &self,
        service: &ContentService,
        instance: &RegisteredInstance,
        canonical_id: &CanonicalId,
        version_id: Option<&str>,
    ) -> Result<super::packs::PackFilePreview, MutationError> {
        let pack = self.resolve_pack(service, canonical_id, version_id).await?;
        let target = super::resolve::validated_target(
            &instance.record().instance.loader_key,
            &instance.record().instance.minecraft_version,
        )
        .map_err(|_| MutationError::Changed)?;
        let mut occupied = Vec::new();
        for name in ["mods", "resourcepacks", "shaderpacks"] {
            let directory = match instance.game_directory().open_directory(&portable(name)?) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(MutationError::Files),
            };
            let listing = directory
                .entries(50_000)
                .map_err(|_| MutationError::Files)?;
            if listing.state() != axial_fs::DirectoryListingState::Complete {
                return Err(MutationError::Capacity);
            }
            for entry in listing.entries() {
                let filename = entry.utf8_name().ok_or(MutationError::Changed)?;
                portable(filename)?;
                occupied.push(format!("{name}/{filename}"));
            }
        }
        Ok(super::packs::preview_files(service, pack, &target, &occupied).await?)
    }

    /// Selected imports retain the same admission through archive identity,
    /// dependency resolution and mutation. The resolver must select the exact
    /// archive payload, never another primary artifact from that version.
    pub async fn install_selected_pack(
        &self,
        service: &ContentService,
        id: &InstanceId,
        canonical_id: &CanonicalId,
        version_id: Option<&str>,
        selection_ids: &[String],
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        let preview = self
            .pack_preview(service, &instance, canonical_id, version_id)
            .await?;
        let selection = preview.select(selection_ids)?;
        let plan = self
            .plan_selected_pack_admitted(service, &instance, selection)
            .await?;
        self.install_admitted(instance, plan, false)
    }

    async fn plan_selected_pack_admitted(
        &self,
        service: &ContentService,
        instance: &RegisteredInstance,
        selection: PackFileSelection,
    ) -> Result<TargetedPlan, MutationError> {
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        let (manifest, live, _) = observe(instance.game_directory())?;
        let state = ContentPlanState::from_instance(instance.record(), &manifest, &live)
            .map_err(|_| MutationError::Changed)?;
        TargetedPlan::resolve_selected_pack(service, state, selection, &manifest, &live)
            .await
            .map_err(|error| match error {
                super::resolve::ResolutionError::Pack(error) => MutationError::Pack(error),
                _ => MutationError::Unavailable,
            })
    }

    pub fn has_unsettled_effects(&self) -> bool {
        if !self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
        {
            return true;
        }
        self.directories
            .registry()
            .storage()
            .read(|connection| -> Result<bool, StorageError> {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM content_batches)",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap_or(true)
    }

    /// Preserve durable restart fences across exit without releasing native effects.
    /// Only the exact, permanently joined task owner may release inert admissions.
    pub fn release_shutdown_admissions(
        &self,
        receipt: &crate::tasks::ShutdownReceipt,
    ) -> Result<(), MutationError> {
        if !receipt.belongs_to(&self.tasks) {
            return Err(MutationError::Unavailable);
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| MutationError::Unavailable)?;
        if pending
            .values()
            .any(|batch| !batch.restart_blocked || !batch.effects.is_empty())
        {
            return Err(MutationError::Pending);
        }
        let released = std::mem::take(&mut *pending);
        drop(pending);
        drop(released);
        Ok(())
    }

    pub fn instance_context(
        &self,
        id: &InstanceId,
    ) -> Result<
        (
            super::resolve::ResolutionTarget,
            ContentManifest,
            LiveManagedContent,
        ),
        MutationError,
    > {
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| MutationError::Unavailable)?;
        let (manifest, live, _) = observe(instance.game_directory())?;
        let target = super::resolve::validated_target(
            &instance.record().instance.loader_key,
            &instance.record().instance.minecraft_version,
        )
        .map_err(|_| MutationError::Changed)?;
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        Ok((target, manifest, live))
    }

    pub fn installed(&self, id: &InstanceId) -> Result<ContentManifest, MutationError> {
        let instance = self
            .directories
            .admit_read(id)
            .map_err(|_| MutationError::Unavailable)?;
        let (manifest, _, _) = observe(instance.game_directory())?;
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        Ok(manifest)
    }

    /// An accepted setup may verify its frozen artifacts offline. Metadata
    /// grants no completion authority until its exact live bytes also match.
    pub(crate) fn installed_content_admitted(
        &self,
        instance: &RegisteredInstance,
        files: &[PlannedFile],
    ) -> Result<bool, MutationError> {
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        let entries = files
            .iter()
            .map(|file| {
                validate_planned_artifact(file.kind, &file.file)?;
                file.entry()
            })
            .collect::<ContentResult<Vec<_>>>()?;
        let mut expected = ContentManifest::default();
        expected.try_upsert_batch(entries)?;
        let (installed, live, _) = observe(instance.game_directory())?;
        let mut complete = true;
        for expected in expected.entries() {
            let Some(actual) = installed.find(expected.canonical_id()) else {
                complete = false;
                continue;
            };
            if actual.provider() != expected.provider()
                || actual.project_id() != expected.project_id()
                || actual.version_id() != expected.version_id()
                || actual.kind() != expected.kind()
                || actual.managed_filename() != expected.managed_filename()
                || actual.sha512() != expected.sha512()
                || actual.size() != expected.size()
                || actual.dependencies() != expected.dependencies()
                || !live.contains(actual)
            {
                return Err(MutationError::Changed);
            }
        }
        Ok(complete)
    }

    pub async fn plan(
        &self,
        service: &ContentService,
        id: &InstanceId,
        selections: &[ResolutionSelection],
    ) -> Result<TargetedPlan, MutationError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        self.plan_admitted(service, &instance, selections).await
    }

    pub async fn plan_admitted(
        &self,
        service: &ContentService,
        instance: &RegisteredInstance,
        selections: &[ResolutionSelection],
    ) -> Result<TargetedPlan, MutationError> {
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        let (manifest, live, _) = observe(instance.game_directory())?;
        let state = ContentPlanState::from_instance(instance.record(), &manifest, &live)
            .map_err(|_| MutationError::Changed)?;
        TargetedPlan::resolve(service, state, selections, &manifest, &live)
            .await
            .map_err(|_| MutationError::Unavailable)
    }

    pub fn install(
        &self,
        plan: TargetedPlan,
        allow_incompatible: bool,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        let instance = self
            .directories
            .admit(plan.state().instance_id())
            .map_err(|_| MutationError::Unavailable)?;
        self.install_admitted(instance, plan, allow_incompatible)
    }

    pub fn install_admitted(
        &self,
        instance: RegisteredInstance,
        plan: TargetedPlan,
        allow_incompatible: bool,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        if &instance.record().instance.id != plan.state().instance_id() {
            return Err(MutationError::Changed);
        }
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let receipt = owner.prepare_install(&instance, &plan, allow_incompatible)?;
                owner.accept_batch(instance, receipt, &cancel).await
            })
            .map_err(|_| MutationError::Unavailable)
    }

    fn prepare_install(
        &self,
        instance: &RegisteredInstance,
        plan: &TargetedPlan,
        allow_incompatible: bool,
    ) -> Result<Receipt, MutationError> {
        let (before, live, raw) = observe(instance.game_directory())?;
        let current = ContentPlanState::from_instance(instance.record(), &before, &live)
            .map_err(|_| MutationError::Changed)?;
        let files = plan
            .authorized_files(&current, allow_incompatible)
            .map_err(|_| MutationError::Changed)?;
        let mut after = before.clone();
        after.try_upsert_batch(
            files
                .iter()
                .map(PlannedFile::entry)
                .collect::<ContentResult<Vec<_>>>()?,
        )?;
        if plan.preserves_enabled_state() {
            for file in &files {
                if let Some(old) = before.find(&file.canonical_id) {
                    after.try_set_enabled(&file.canonical_id, old.enabled())?;
                }
            }
        }
        let sources = files
            .into_iter()
            .map(|file| {
                Ok((
                    entry_path(
                        after
                            .find(&file.canonical_id)
                            .ok_or(MutationError::Changed)?,
                    )?,
                    Source::Download { file: file.file },
                ))
            })
            .collect::<Result<HashMap<_, _>, MutationError>>()?;
        self.prepare_batch(instance, before, raw, after, sources)
    }

    pub fn remove(
        &self,
        id: &InstanceId,
        content: &CanonicalId,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        self.remove_many(id, std::slice::from_ref(content))
    }

    pub fn remove_many(
        &self,
        id: &InstanceId,
        ids: &[CanonicalId],
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        if ids.is_empty()
            || ids.len() > 256
            || ids.iter().collect::<HashSet<_>>().len() != ids.len()
        {
            return Err(MutationError::Changed);
        }
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        let ids = ids.to_vec();
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let (before, live, raw) = observe(instance.game_directory())?;
                let mut after = before.clone();
                for id in &ids {
                    let entry = before.find(id).ok_or(MutationError::NotFound)?;
                    if entry.kind() != ContentKind::Modpack
                        && !live.contains(entry)
                        && observe_path(instance.game_directory(), &entry_path(entry)?)?.is_some()
                    {
                        return Err(MutationError::Changed);
                    }
                    after.remove(id);
                }
                for id in &ids {
                    let old = before.find(id).ok_or(MutationError::NotFound)?;
                    if after.entries().iter().any(|entry| {
                        entry.enabled()
                            && live.contains(entry)
                            && entry.dependencies().iter().any(|dependency| {
                                dependency.requires_project(old.project_id(), old.version_id())
                            })
                    }) {
                        return Err(MutationError::Required);
                    }
                }
                owner
                    .begin(instance, before, raw, after, HashMap::new(), &cancel)
                    .await
            })
            .map_err(|_| MutationError::Unavailable)
    }

    pub fn set_enabled(
        &self,
        id: &InstanceId,
        content: &CanonicalId,
        enabled: bool,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        self.edit(id, content, Some(enabled))
    }

    fn edit(
        &self,
        id: &InstanceId,
        content: &CanonicalId,
        enabled: Option<bool>,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        let instance = self
            .directories
            .admit(id)
            .map_err(|_| MutationError::Unavailable)?;
        let content = content.clone();
        let owner = self.clone();
        self.tasks
            .try_spawn(instance.clone(), move |cancel| async move {
                let (before, live, raw) = observe(instance.game_directory())?;
                let old = before.find(&content).ok_or(MutationError::NotFound)?;
                if old.kind() == ContentKind::Modpack {
                    return Err(MutationError::Conflict);
                }
                if !live.contains(old) {
                    return Err(MutationError::Changed);
                }
                let mut after = before.clone();
                let mut sources = HashMap::new();
                if let Some(enabled) = enabled {
                    after.try_set_enabled(&content, enabled)?;
                    let new = after.find(&content).ok_or(MutationError::NotFound)?;
                    if old.enabled() != enabled {
                        sources.insert(
                            entry_path(new)?,
                            Source::Local {
                                path: entry_path(old)?,
                                proof: Proof::entry(old)?,
                            },
                        );
                    }
                } else {
                    after.remove(&content);
                }
                owner
                    .begin(instance, before, raw, after, sources, &cancel)
                    .await
            })
            .map_err(|_| MutationError::Unavailable)
    }

    pub(crate) async fn change_local_mod_admitted(
        &self,
        instance: RegisteredInstance,
        from: &PortableName,
        to: Option<&PortableName>,
        cancel: &CancellationToken,
    ) -> Result<bool, MutationError> {
        let Some(receipt) = self.plan_local_mod_change(&instance, from, to)? else {
            return Ok(false);
        };
        self.accept_batch(instance, receipt, cancel).await?;
        Ok(true)
    }

    fn plan_local_mod_change(
        &self,
        instance: &RegisteredInstance,
        from: &PortableName,
        to: Option<&PortableName>,
    ) -> Result<Option<Receipt>, MutationError> {
        let raw = read_path(instance.game_directory(), MANIFEST_FILE)?;
        let before = ContentManifest::decode_managed(raw.as_deref())?;
        let key = crate::files::portable::managed_content_name_key(from);
        let Some(entry) = before.entries().iter().find(|entry| {
            entry.kind() == ContentKind::Mod
                && entry
                    .managed_filename()
                    .is_some_and(|name| name.key() == key)
        }) else {
            return Ok(None);
        };
        let intent = LocalModIntent {
            source: format!("mods/{}", from.as_str()),
            destination: to.map(|name| format!("mods/{}", name.as_str())),
        };
        let proof = observe_path(instance.game_directory(), &intent.source)?
            .ok_or(MutationError::NotFound)?;
        if proof == Proof::entry(entry)? {
            return Err(MutationError::Managed);
        }
        if to == Some(from) {
            return Ok(None);
        }
        let mut changes = Vec::new();
        if let Some(destination) = &intent.destination {
            if observe_path(instance.game_directory(), destination)?.is_some() {
                return Err(MutationError::Conflict);
            }
            changes.push(Change {
                path: destination.clone(),
                before: None,
                after: Some(proof.clone()),
                source: Some(Source::Local {
                    path: intent.source.clone(),
                    proof: proof.clone(),
                }),
            });
        }
        changes.push(Change {
            path: intent.source.clone(),
            before: Some(proof),
            after: None,
            source: None,
        });
        let receipt = Receipt {
            schema: 1,
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.record().instance.id.clone(),
            instance_revision: instance.record().revision,
            directory_receipt: instance
                .game_directory()
                .receipt()
                .map_err(|_| MutationError::Changed)?,
            after_manifest: raw.clone().ok_or(MutationError::Changed)?,
            before_manifest: raw,
            before_observed_manifest: before.encode_managed()?,
            changes,
            native_settled: false,
            pack: None,
            local_mod: Some(intent),
            ready_checkpoint: None,
        };
        validate_receipt(&receipt)?;
        Ok(Some(receipt))
    }

    async fn begin(
        &self,
        instance: RegisteredInstance,
        before: ContentManifest,
        raw: Option<Vec<u8>>,
        after: ContentManifest,
        sources: HashMap<String, Source>,
        cancel: &CancellationToken,
    ) -> Result<MutationReceipt, MutationError> {
        let receipt = self.prepare_batch(&instance, before, raw, after, sources)?;
        self.accept_batch(instance, receipt, cancel).await
    }

    fn prepare_batch(
        &self,
        instance: &RegisteredInstance,
        before: ContentManifest,
        raw: Option<Vec<u8>>,
        after: ContentManifest,
        sources: HashMap<String, Source>,
    ) -> Result<Receipt, MutationError> {
        let old = manifest_paths(&before)?;
        let new = manifest_paths(&after)?;
        let mut paths: Vec<_> = old.keys().chain(new.keys()).cloned().collect();
        paths.sort();
        paths.dedup();
        let mut changes = Vec::new();
        for path in paths {
            if old.get(&path) == new.get(&path) {
                continue;
            }
            let before = observe_path(instance.game_directory(), &path)?;
            if before.is_some() && before.as_ref() != old.get(&path) {
                return Err(MutationError::Changed);
            }
            let after = new.get(&path).cloned();
            if before != after {
                changes.push(Change {
                    before,
                    after,
                    source: sources.get(&path).cloned(),
                    path,
                });
            }
        }
        // Publish additions before removing displaced names, including toggles.
        changes.sort_by_key(|change| change.after.is_none());
        let receipt = Receipt {
            schema: 1,
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.record().instance.id.clone(),
            instance_revision: instance.record().revision,
            directory_receipt: instance
                .game_directory()
                .receipt()
                .map_err(|_| MutationError::Changed)?,
            before_manifest: raw,
            before_observed_manifest: before.encode_managed()?,
            after_manifest: after.encode_managed()?,
            changes,
            native_settled: false,
            pack: None,
            local_mod: None,
            ready_checkpoint: None,
        };
        Ok(receipt)
    }

    async fn accept_batch(
        &self,
        instance: RegisteredInstance,
        receipt: Receipt,
        cancel: &CancellationToken,
    ) -> Result<MutationReceipt, MutationError> {
        validate_receipt(&receipt)?;
        for change in &receipt.changes {
            if let Some(name) = change.path.strip_prefix("mods/") {
                self.protect_performance_mod(&instance, name).await?;
            }
        }
        if cancel.is_cancelled() {
            return Err(MutationError::Cancelled);
        }
        instance
            .validate_current()
            .map_err(|_| MutationError::Changed)?;
        preflight(instance.game_directory(), &receipt, false)?;
        self.persist_receipt(&receipt)?;
        self.apply(instance, receipt, HashMap::new(), cancel).await
    }

    fn persist_receipt(&self, receipt: &Receipt) -> Result<(), MutationError> {
        let json = encoded_receipt(receipt)?;
        self.directories.registry().storage().transaction(|tx| -> Result<(), MutationError> {
            let inserted = tx.execute("INSERT INTO content_batches(instance_id, operation_id, receipt_json) VALUES(?1, ?2, ?3)",
                params![receipt.instance_id.as_str(), receipt.operation_id, json])?;
            if inserted != 1 {
                return Err(MutationError::Changed);
            }
            let persisted: Option<(String, String)> = tx.query_row(
                "SELECT operation_id,receipt_json FROM content_batches WHERE instance_id=?1 AND length(CAST(receipt_json AS BLOB))<=?2",
                params![receipt.instance_id.as_str(), MAX_RECEIPT_BYTES], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            if persisted.as_ref().is_none_or(|(operation, payload)| operation != &receipt.operation_id || payload != &json) {
                return Err(MutationError::Changed);
            }
            Ok(())
        })
    }

    fn replace_receipt(&self, receipt: &Receipt, previous: &str) -> Result<(), MutationError> {
        let json = encoded_receipt(receipt)?;
        self.directories.registry().storage().transaction(|tx| -> Result<(), MutationError> {
            if tx.execute("UPDATE content_batches SET receipt_json=?1 WHERE instance_id=?2 AND operation_id=?3 AND receipt_json=?4",
                params![json, receipt.instance_id.as_str(), receipt.operation_id, previous])? != 1 {
                return Err(MutationError::Changed);
            }
            let stored: Option<String> = tx.query_row("SELECT receipt_json FROM content_batches WHERE instance_id=?1 AND operation_id=?2 AND length(CAST(receipt_json AS BLOB))<=?3",
                params![receipt.instance_id.as_str(), receipt.operation_id, MAX_RECEIPT_BYTES], |row| row.get(0)).optional()?;
            if stored.as_deref() != Some(json.as_str()) {
                return Err(MutationError::Changed);
            }
            Ok(())
        })
    }

    fn persist_checkpoint(
        &self,
        receipt: &mut Receipt,
        checkpoint: Result<Option<&ManagedContentStagingCheckpoint>, ManagedContentCheckpointError>,
    ) -> Result<bool, MutationError> {
        let checkpoint = match checkpoint {
            Ok(Some(checkpoint)) => checkpoint,
            Ok(None) => return Ok(false),
            Err(
                ManagedContentCheckpointError::Unsupported
                | ManagedContentCheckpointError::Capacity,
            ) => return Ok(false),
            Err(_) => return Err(MutationError::Changed),
        };
        let previous = encoded_receipt(receipt)?;
        let budget = match checkpoint_budget(receipt) {
            Ok(budget) => budget,
            Err(MutationError::Capacity) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut next = receipt.clone();
        if budget == 0 {
            return Ok(false);
        }
        let checkpoint = match checkpoint.encode(budget) {
            Ok(checkpoint) => checkpoint,
            Err(
                ManagedContentCheckpointError::Unsupported
                | ManagedContentCheckpointError::Capacity,
            ) => return Ok(false),
            Err(_) => return Err(MutationError::Changed),
        };
        next.ready_checkpoint = Some(checkpoint);
        match encoded_receipt(&next) {
            Ok(_) => {}
            Err(MutationError::Capacity) => return Ok(false),
            Err(error) => return Err(error),
        }
        self.replace_receipt(&next, &previous)?;
        *receipt = next;
        Ok(true)
    }

    fn reconcile_recovery(
        &self,
        receipt: &mut Receipt,
        recovery: ManagedContentRecovery,
    ) -> ManagedContentTransactionOutcome {
        let budget = match checkpoint_budget(receipt) {
            Ok(budget) => budget,
            Err(MutationError::Capacity) => 0,
            Err(_) => return ManagedContentTransactionOutcome::RecoveryRequired(recovery),
        };
        recovery.reconcile_with_checkpoint(budget, |checkpoint| {
            matches!(
                self.persist_checkpoint(receipt, Ok(Some(checkpoint))),
                Ok(true)
            )
        })
    }

    pub fn resume(
        &self,
        id: &InstanceId,
    ) -> Result<TaskHandle<Result<MutationReceipt, MutationError>>, MutationError> {
        if !self
            .resuming
            .lock()
            .map_err(|_| MutationError::Unavailable)?
            .insert(id.clone())
        {
            return Err(MutationError::Pending);
        }
        let resume_guard = ResumeGuard {
            id: id.clone(),
            resuming: self.resuming.clone(),
        };
        let retained = self
            .pending
            .lock()
            .map_err(|_| MutationError::Unavailable)?
            .get(id)
            .map(|batch| batch.instance.clone());
        let instance = match retained {
            Some(instance) => instance,
            None => self
                .directories
                .admit_content_settlement(id)
                .map_err(|_| MutationError::Unavailable)?,
        };
        let owner = self.clone();
        self.tasks.try_spawn((instance.clone(), resume_guard), move |_cancel| async move {
            let loaded = (|| {
                let raw: Option<String> = owner.directories.registry().storage().read(|connection| -> Result<_, MutationError> {
                    let id = instance.record().instance.id.as_str();
                    let raw = connection.query_row("SELECT receipt_json FROM content_batches WHERE instance_id=?1 AND length(CAST(receipt_json AS BLOB))<=?2",
                        params![id, MAX_RECEIPT_BYTES], |row| row.get::<_, String>(0)).optional()?;
                    if raw.is_none() && !connection.query_row("SELECT EXISTS(SELECT 1 FROM content_batches WHERE instance_id=?1)", [id], |row| row.get::<_, bool>(0))? {
                        return Err(MutationError::NotFound);
                    }
                    Ok(raw)
                })?;
                let raw = raw.ok_or(MutationError::Pending)?;
                let receipt: Receipt = serde_json::from_str(&raw).map_err(|_| MutationError::Changed)?;
                validate_receipt(&receipt)?;
                validate_instance(&instance, &receipt)?;
                Ok::<_, MutationError>(receipt)
            })();
            let mut receipt = match loaded {
                Ok(receipt) => receipt,
                Err(error) => {
                    if !matches!(error, MutationError::NotFound) {
                        owner.pending.lock().map_err(|_| MutationError::Unavailable)?
                            .entry(instance.record().instance.id.clone())
                            .or_insert(PendingBatch { instance, effects: Vec::new(), restart_blocked: true });
                    }
                    return Err(error);
                }
            };
            let batch = owner.pending.lock().map_err(|_| MutationError::Unavailable)?.remove(&instance.record().instance.id);
            let restarting = batch.as_ref().is_none_or(|batch| batch.restart_blocked);
            let effects = batch.map(|batch| batch.effects).unwrap_or_default();
            let mut remaining: Vec<_> = effects.into_iter().map(|effect| effect.settle(&owner, &mut receipt)).collect();
            if remaining.iter().any(|effect| matches!(effect, Effect::Recovery(_))) { return owner.retain(instance, remaining); }
            let raw = match encoded_receipt(&receipt) {
                Ok(raw) => raw,
                Err(_) => return owner.retain_blocked(instance, remaining, restarting),
            };
            if !restarting && !remaining.is_empty()
                && remaining.iter().all(|effect| matches!(effect, Effect::RolledBack))
                && preflight(instance.game_directory(), &receipt, false).is_ok()
            {
                if owner.clear_receipt(&receipt, &raw).is_err() { return owner.retain(instance, remaining); }
                return Err(MutationError::Cancelled);
            }
            if remaining.iter().any(|effect| matches!(effect, Effect::Committed)) {
                if owner.mark_native_settled(&mut receipt, &raw).is_err() { return owner.retain(instance, remaining); }
                remaining.clear();
            }
            if restarting && !receipt.native_settled {
                let recovery = (|| {
                    let checkpoint = receipt.ready_checkpoint.as_deref().ok_or(MutationError::Pending)?;
                    let checkpoint = ManagedContentStagingCheckpoint::decode(checkpoint).map_err(|_| MutationError::Pending)?;
                    transaction_root(&instance, receipt.pack.is_some())?
                        .restore_staging_checkpoint(checkpoint, checkpoint_binding(&receipt)?)
                        .map_err(|_| MutationError::Pending)
                })();
                let Ok(recovery) = recovery else { return owner.retain_blocked(instance, Vec::new(), true); };
                match owner.reconcile_recovery(&mut receipt, recovery) {
                    ManagedContentTransactionOutcome::Cancelled(_) => {
                        if preflight(instance.game_directory(), &receipt, false).is_ok()
                            && encoded_receipt(&receipt).is_ok_and(|current| owner.clear_receipt(&receipt, &current).is_ok())
                        { return Err(MutationError::Cancelled); }
                        return owner.retain_blocked(instance, Vec::new(), true);
                    }
                    ManagedContentTransactionOutcome::RecoveryRequired(effect) => return owner.retain(instance, vec![Effect::Recovery(effect)]),
                    ManagedContentTransactionOutcome::Committed(_) | ManagedContentTransactionOutcome::Failed(_) => return owner.retain_blocked(instance, Vec::new(), true),
                }
            }
            owner.apply(instance, receipt, HashMap::new(), &CancellationToken::new()).await
        }).map_err(|_| MutationError::Unavailable)
    }

    async fn apply(
        &self,
        instance: RegisteredInstance,
        receipt: Receipt,
        payloads: HashMap<String, Vec<u8>>,
        cancel: &CancellationToken,
    ) -> Result<MutationReceipt, MutationError> {
        self.apply_pack(instance, receipt, payloads, None, cancel)
            .await
    }

    async fn apply_pack(
        &self,
        instance: RegisteredInstance,
        mut receipt: Receipt,
        mut payloads: HashMap<String, Vec<u8>>,
        pack: Option<&PackPlan>,
        cancel: &CancellationToken,
    ) -> Result<MutationReceipt, MutationError> {
        let mut effects = Vec::new();
        let result = async {
            validate_instance(&instance, &receipt)?;
            preflight(instance.game_directory(), &receipt, true)?;
            if !receipt.native_settled {
                apply_streamed(
                    self,
                    &instance,
                    &mut receipt,
                    &mut payloads,
                    pack,
                    &mut effects,
                    cancel,
                    self.progress.as_deref(),
                )
                .await?;
                effects.push(Effect::Committed);
                let previous = encoded_receipt(&receipt)?;
                self.mark_native_settled(&mut receipt, &previous)?;
                effects.clear();
            }
            // Verify the actual filesystem before discarding the settlement fence.
            for change in &receipt.changes {
                if observe_path(instance.game_directory(), &change.path)? != change.after {
                    return Err(MutationError::Changed);
                }
            }
            if read_path(instance.game_directory(), MANIFEST_FILE)?.as_deref()
                != Some(receipt.after_manifest.as_slice())
            {
                return Err(MutationError::Changed);
            }
            self.clear_receipt(&receipt, &encoded_receipt(&receipt)?)?;
            report_progress(self.progress.as_deref(), "content_commit", 1, 1, None);
            Ok(MutationReceipt {
                operation_id: receipt.operation_id.clone(),
                instance_id: receipt.instance_id.clone(),
                status: "complete",
                changed_files: receipt.changes.len(),
            })
        }
        .await;
        match result {
            Ok(result) => Ok(result),
            Err(error)
                if effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::RolledBack))
                    && !effects.iter().any(|effect| {
                        matches!(effect, Effect::Recovery(_) | Effect::Committed)
                    })
                    && preflight(instance.game_directory(), &receipt, false).is_ok() =>
            {
                if self
                    .clear_receipt(&receipt, &encoded_receipt(&receipt)?)
                    .is_err()
                {
                    return self.retain(instance, effects);
                }
                Err(error)
            }
            Err(_) => self.retain(instance, effects),
        }
    }

    fn clear_receipt(&self, receipt: &Receipt, previous: &str) -> Result<(), MutationError> {
        self.directories
            .registry()
            .storage()
            .transaction(|tx| -> Result<(), MutationError> {
                if tx.execute(
                    "DELETE FROM content_batches WHERE instance_id = ?1 AND operation_id = ?2 AND receipt_json = ?3",
                    params![receipt.instance_id.as_str(), receipt.operation_id, previous],
                )? != 1
                {
                    return Err(MutationError::Changed);
                }
                let remains: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM content_batches WHERE instance_id=?1)",
                    [receipt.instance_id.as_str()], |row| row.get(0),
                )?;
                if remains { return Err(MutationError::Changed); }
                Ok(())
            })
    }

    fn mark_native_settled(
        &self,
        receipt: &mut Receipt,
        previous: &str,
    ) -> Result<(), MutationError> {
        let mut next = receipt.clone();
        next.native_settled = true;
        self.replace_receipt(&next, previous)?;
        *receipt = next;
        Ok(())
    }

    fn retain(
        &self,
        instance: RegisteredInstance,
        effects: Vec<Effect>,
    ) -> Result<MutationReceipt, MutationError> {
        self.retain_blocked(instance, effects, false)
    }

    fn retain_blocked(
        &self,
        instance: RegisteredInstance,
        effects: Vec<Effect>,
        restart_blocked: bool,
    ) -> Result<MutationReceipt, MutationError> {
        self.pending
            .lock()
            .map_err(|_| MutationError::Unavailable)?
            .insert(
                instance.record().instance.id.clone(),
                PendingBatch {
                    instance,
                    effects,
                    restart_blocked,
                },
            );
        Err(MutationError::Pending)
    }
}

struct ResumeGuard {
    id: InstanceId,
    resuming: Arc<Mutex<HashSet<InstanceId>>>,
}
impl Drop for ResumeGuard {
    fn drop(&mut self) {
        self.resuming
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.id);
    }
}

/// Only a provider hash match grants direct managed-file provenance. Nested
/// files, overrides and unrecognized members remain pack payloads.
async fn identify_pack_members(
    service: &ContentService,
    plan: &PackPlan,
    changes: &[Change],
    pack_id: &CanonicalId,
    cancel: &CancellationToken,
) -> Result<Vec<ManifestEntry>, MutationError> {
    let mut by_hash: HashMap<&str, Vec<(ContentKind, &FileRef)>> = HashMap::new();
    for (member, change) in plan.files().iter().zip(changes) {
        let (Some(kind), Some(hash)) = (member.kind(), member.sha512.as_deref()) else {
            continue;
        };
        let Some(Source::PackDownload { file }) = &change.source else {
            return Err(MutationError::Changed);
        };
        by_hash.entry(hash).or_default().push((kind, file));
    }
    if by_hash.is_empty() {
        return Ok(Vec::new());
    }
    let mut hashes = by_hash
        .keys()
        .map(|hash| (*hash).to_string())
        .collect::<Vec<_>>();
    hashes.sort();
    let service = service.with_cancellation(cancel.clone());
    let mut identities = HashMap::new();
    for hashes in hashes.chunks(super::provider::MAX_PROVIDER_BATCH_ITEMS) {
        identities.extend(service.identify(hashes).await.map_err(|_| {
            if cancel.is_cancelled() {
                MutationError::Cancelled
            } else {
                MutationError::Pack(super::packs::PackError::Provider)
            }
        })?);
    }
    let mut ids = HashSet::new();
    for (hash, identity) in &identities {
        let id = CanonicalId::for_project(identity.provider, &identity.project_id);
        if &id == pack_id
            || !ids.insert(id)
            || by_hash
                .get(hash.as_str())
                .is_none_or(|files| files.len() != 1)
        {
            return Err(MutationError::Conflict);
        }
    }
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    // Display titles do not determine authenticated ownership or publication.
    let mut titles = HashMap::new();
    for ids in ids.chunks(super::provider::MAX_PROVIDER_BATCH_ITEMS) {
        titles.extend(service.metadata(ids).await.unwrap_or_default());
    }
    if cancel.is_cancelled() {
        return Err(MutationError::Cancelled);
    }
    let mut entries = Vec::new();
    for hash in hashes {
        let Some(identity) = identities.get(&hash) else {
            continue;
        };
        let (kind, file) = by_hash[hash.as_str()][0];
        let id = CanonicalId::for_project(identity.provider, &identity.project_id);
        entries.push(ManifestEntry::managed(
            id.clone(),
            identity.provider,
            identity.project_id.clone(),
            identity.version_id.clone(),
            kind,
            file,
            identity.dependencies.clone(),
            titles
                .get(&id)
                .map(|metadata| metadata.title.clone())
                .or_else(|| identity.title.clone()),
        )?);
    }
    Ok(entries)
}

pub(crate) fn observe(
    game: &ScopedDirectory,
) -> Result<(ContentManifest, LiveManagedContent, Option<Vec<u8>>), MutationError> {
    let raw = read_path(game, MANIFEST_FILE)?;
    let mut manifest = ContentManifest::decode_managed(raw.as_deref())?;
    let mut verified = HashSet::new();
    for entry in manifest.entries().to_vec() {
        if entry.kind() == ContentKind::Modpack {
            continue;
        }
        let path = entry_path(&entry)?;
        let alternate = path
            .strip_suffix(".disabled")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{path}.disabled"));
        let current = observe_path(game, &path)?;
        let other = observe_path(game, &alternate)?;
        let proof = Proof::entry(&entry)?;
        if current.as_ref() == Some(&proof) && other.is_none() {
            verified.insert(entry.canonical_id().clone());
        } else if current.is_none() && other.as_ref() == Some(&proof) {
            manifest.try_set_enabled(entry.canonical_id(), !entry.enabled())?;
            verified.insert(entry.canonical_id().clone());
        }
    }
    let live = LiveManagedContent::from_entries(
        manifest
            .entries()
            .iter()
            .filter(|entry| verified.contains(entry.canonical_id())),
    );
    Ok((manifest, live, raw))
}

fn manifest_paths(manifest: &ContentManifest) -> Result<HashMap<String, Proof>, MutationError> {
    manifest
        .entries()
        .iter()
        .filter(|entry| entry.kind() != ContentKind::Modpack)
        .map(|entry| Ok((entry_path(entry)?, Proof::entry(entry)?)))
        .collect()
}

fn entry_path(entry: &ManifestEntry) -> Result<String, MutationError> {
    let directory = entry
        .kind()
        .install_subdir()
        .ok_or(MutationError::Changed)?;
    let name = entry.managed_filename().ok_or(MutationError::Changed)?;
    Ok(format!(
        "{directory}/{}",
        if entry.enabled() {
            name.as_str()
        } else {
            name.disabled().as_str()
        }
    ))
}

fn portable(name: &str) -> Result<PortableName, MutationError> {
    PortableName::new_exact(name).map_err(|_| MutationError::Changed)
}

fn read_path(game: &ScopedDirectory, path: &str) -> Result<Option<Vec<u8>>, MutationError> {
    let path = crate::files::ScopedPath::new_exact(path).map_err(|_| MutationError::Changed)?;
    let limit = if path.as_str() == MANIFEST_FILE {
        MAX_MANIFEST_BYTES as u64
    } else {
        MAX_CONTENT_FILE_BYTES
    };
    match game.read_bounded(&path, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(MutationError::Files),
    }
}

/// Hash payloads through the exact retained file reader, not a payload-sized
/// allocation. Finishing the reader proves the observed revision stayed fixed.
fn observe_path(game: &ScopedDirectory, path: &str) -> Result<Option<Proof>, MutationError> {
    let path = crate::files::ScopedPath::new_exact(path).map_err(|_| MutationError::Changed)?;
    let (parent, name) = path
        .as_str()
        .rsplit_once('/')
        .unwrap_or(("", path.as_str()));
    let file = (|| {
        let parent = if parent.is_empty() {
            game.clone()
        } else {
            game.directory(
                &crate::files::ScopedPath::new_exact(parent)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
            )?
        };
        parent.open_file(&portable(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?)
    })();
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(MutationError::Files),
    };
    let mut reader = file
        .reader(MAX_CONTENT_FILE_BYTES)
        .map_err(|_| MutationError::Files)?;
    let mut digest = Sha512::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = io::Read::read(&mut reader, &mut buffer).map_err(|_| MutationError::Files)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .filter(|size| *size <= MAX_CONTENT_FILE_BYTES)
            .ok_or(MutationError::Capacity)?;
        digest.update(&buffer[..read]);
    }
    reader.finish().map_err(|_| MutationError::Changed)?;
    Ok(Some(Proof {
        size,
        sha512: format!("{:x}", digest.finalize()),
    }))
}

fn validate_instance(
    instance: &RegisteredInstance,
    receipt: &Receipt,
) -> Result<(), MutationError> {
    instance
        .validate_current()
        .map_err(|_| MutationError::Changed)?;
    if instance.record().instance.id != receipt.instance_id
        || instance.record().revision != receipt.instance_revision
    {
        return Err(MutationError::Changed);
    }
    instance
        .game_directory()
        .verify_receipt(&receipt.directory_receipt)
        .map_err(|_| MutationError::Changed)
}

fn validate_receipt(receipt: &Receipt) -> Result<(), MutationError> {
    if receipt.schema != 1
        || uuid::Uuid::parse_str(&receipt.operation_id).is_err()
        || receipt.changes.len()
            > if receipt.pack.is_some() {
                20_000 + 8192
            } else {
                8192
            }
    {
        return Err(MutationError::Changed);
    }
    let mut recorded = ContentManifest::decode_managed(receipt.before_manifest.as_deref())?;
    let before = ContentManifest::decode_managed(Some(&receipt.before_observed_manifest))?;
    if let Some(intent) = &receipt.local_mod {
        return validate_local_mod_receipt(receipt, intent, &recorded, &before);
    }
    // Observation may correct only enabled/disabled spelling. It cannot invent
    // provenance or change a provider identity, digest, dependency, or filename.
    for entry in before.entries() {
        recorded.try_set_enabled(entry.canonical_id(), entry.enabled())?;
    }
    if recorded != before {
        return Err(MutationError::Changed);
    }
    let after = ContentManifest::decode_managed(Some(&receipt.after_manifest))?;
    if let Some(pack) = &receipt.pack {
        return validate_pack_receipt(receipt, pack, &before, &after);
    }
    let before = manifest_paths(&before)?;
    let after = manifest_paths(&after)?;
    let mut paths = HashSet::new();
    let mut bytes = 0_u64;
    for change in &receipt.changes {
        if !paths.insert(&change.path)
            || change
                .before
                .as_ref()
                .is_some_and(|proof| before.get(&change.path) != Some(proof))
            || change.after != after.get(&change.path).cloned()
            || change.before == change.after
        {
            return Err(MutationError::Changed);
        }
        if let Some(proof) = &change.after {
            bytes = bytes
                .checked_add(proof.size)
                .filter(|bytes| *bytes <= MAX_BATCH_BYTES)
                .ok_or(MutationError::Capacity)?;
            match change.source.as_ref().ok_or(MutationError::Changed)? {
                Source::Download { file } => {
                    let kind = if change.path.starts_with("mods/") {
                        ContentKind::Mod
                    } else if change.path.starts_with("resourcepacks/") {
                        ContentKind::ResourcePack
                    } else {
                        ContentKind::ShaderPack
                    };
                    validate_planned_artifact(kind, file)?;
                    if file.size != Some(proof.size) || file.sha512.as_ref() != Some(&proof.sha512)
                    {
                        return Err(MutationError::Changed);
                    }
                }
                Source::Local {
                    path,
                    proof: source,
                } => {
                    if before.get(path) != Some(source) || source != proof {
                        return Err(MutationError::Changed);
                    }
                }
                Source::PackDownload { .. } | Source::PackOverride => {
                    return Err(MutationError::Changed);
                }
            }
        } else if change.source.is_some() {
            return Err(MutationError::Changed);
        }
    }
    for path in before.keys().chain(after.keys()) {
        if before.get(path) != after.get(path) && after.contains_key(path) && !paths.contains(path)
        {
            return Err(MutationError::Changed);
        }
    }
    Ok(())
}

fn validate_local_mod_receipt(
    receipt: &Receipt,
    intent: &LocalModIntent,
    recorded: &ContentManifest,
    before: &ContentManifest,
) -> Result<(), MutationError> {
    if receipt.pack.is_some()
        || recorded != before
        || receipt.before_manifest.as_deref() != Some(receipt.after_manifest.as_slice())
        || receipt.changes.len() != 1 + usize::from(intent.destination.is_some())
    {
        return Err(MutationError::Changed);
    }
    let source_name = portable(
        intent
            .source
            .strip_prefix("mods/")
            .ok_or(MutationError::Changed)?,
    )?;
    let source_key = source_name.key();
    let alternate = if source_key.as_str().ends_with(".jar.disabled") {
        intent.source[..intent.source.len() - ".disabled".len()].to_string()
    } else if source_key.as_str().ends_with(".jar") {
        format!("{}.disabled", intent.source)
    } else {
        return Err(MutationError::Changed);
    };
    if intent
        .destination
        .as_ref()
        .is_some_and(|path| path != &alternate)
    {
        return Err(MutationError::Changed);
    }
    let key = crate::files::portable::managed_content_name_key(&source_name);
    let entry = recorded
        .entries()
        .iter()
        .find(|entry| {
            entry.kind() == ContentKind::Mod
                && entry
                    .managed_filename()
                    .is_some_and(|name| name.key() == key)
        })
        .ok_or(MutationError::Changed)?;
    let source = receipt
        .changes
        .iter()
        .find(|change| change.path == intent.source)
        .ok_or(MutationError::Changed)?;
    let proof = source.before.as_ref().ok_or(MutationError::Changed)?;
    if source.after.is_some()
        || source.source.is_some()
        || proof.size > MAX_CONTENT_FILE_BYTES
        || !super::provenance::valid_sha512(&proof.sha512)
        || proof == &Proof::entry(entry)?
    {
        return Err(MutationError::Changed);
    }
    if let Some(destination) = &intent.destination {
        let target = receipt
            .changes
            .iter()
            .find(|change| &change.path == destination)
            .ok_or(MutationError::Changed)?;
        if target.before.is_some()
            || target.after.as_ref() != Some(proof)
            || !matches!(&target.source, Some(Source::Local { path, proof: copied }) if path == &intent.source && copied == proof)
        {
            return Err(MutationError::Changed);
        }
    }
    Ok(())
}

fn validate_pack_receipt(
    receipt: &Receipt,
    canonical_id: &CanonicalId,
    before: &ContentManifest,
    after: &ContentManifest,
) -> Result<(), MutationError> {
    let entry = after.find(canonical_id).ok_or(MutationError::Changed)?;
    let installation = entry.pack_installation().ok_or(MutationError::Changed)?;
    if before
        .find(canonical_id)
        .is_some_and(|entry| entry.kind() != ContentKind::Modpack)
        || installation.files.len() > receipt.changes.len()
    {
        return Err(MutationError::Changed);
    }
    let mut expected = before.clone();
    let mut additions = vec![entry.clone()];
    let changes_by_path = receipt
        .changes
        .iter()
        .map(|change| (change.path.as_str(), change))
        .collect::<HashMap<_, _>>();
    if changes_by_path.len() != receipt.changes.len() {
        return Err(MutationError::Changed);
    }
    for entry in after.entries() {
        if entry.canonical_id() == canonical_id || before.find(entry.canonical_id()) == Some(entry)
        {
            continue;
        }
        if !entry.enabled() || entry.kind() == ContentKind::Modpack {
            return Err(MutationError::Changed);
        }
        let path = entry_path(entry)?;
        let change = changes_by_path
            .get(path.as_str())
            .ok_or(MutationError::Changed)?;
        if !matches!(change.source, Some(Source::PackDownload { .. }))
            || change.after.as_ref() != Some(&Proof::entry(entry)?)
        {
            return Err(MutationError::Changed);
        }
        additions.push(entry.clone());
    }
    expected.try_upsert_batch(additions)?;
    if &expected != after {
        return Err(MutationError::Changed);
    }
    let mut override_bytes = 0u64;
    for (change, installed) in receipt.changes.iter().zip(&installation.files) {
        let proof = change.after.as_ref().ok_or(MutationError::Changed)?;
        if change.path != installed.path
            || change.before.is_some()
            || proof.size != installed.size
            || proof.sha512 != installed.sha512
        {
            return Err(MutationError::Changed);
        }
        match change.source.as_ref() {
            Some(Source::PackDownload { file }) => {
                if file.filename
                    != change
                        .path
                        .rsplit('/')
                        .next()
                        .ok_or(MutationError::Changed)?
                    || file.size != Some(proof.size)
                    || file.sha512.as_ref() != Some(&proof.sha512)
                    || file.sha1.as_ref().is_some_and(|hash| {
                        hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                {
                    return Err(MutationError::Changed);
                }
                validate_download_url(&file.url)?;
            }
            Some(Source::PackOverride) => {
                override_bytes = override_bytes
                    .checked_add(proof.size)
                    .ok_or(MutationError::Capacity)?;
                if proof.size > super::packs::MAX_OVERRIDE_ENTRY_BYTES
                    || override_bytes > super::packs::MAX_OVERRIDE_TOTAL_BYTES
                {
                    return Err(MutationError::Capacity);
                }
            }
            _ => return Err(MutationError::Changed),
        }
    }
    let stale = pack_stale_paths(before, after, canonical_id)?;
    for change in &receipt.changes[installation.files.len()..] {
        if change.after.is_some()
            || change.source.is_some()
            || change.before.as_ref() != stale.get(&change.path)
            || change.before.is_none()
        {
            return Err(MutationError::Changed);
        }
    }
    Ok(())
}

/// A renamed member can retire both spellings, but pack destinations and
/// projects absent from the new pack never become cleanup authority.
fn pack_stale_paths(
    before: &ContentManifest,
    after: &ContentManifest,
    pack_id: &CanonicalId,
) -> Result<BTreeMap<String, Proof>, MutationError> {
    let installation = after
        .find(pack_id)
        .and_then(ManifestEntry::pack_installation)
        .ok_or(MutationError::Changed)?;
    let protected = installation
        .files
        .iter()
        .map(|file| {
            ScopedPath::new_exact(&file.path)
                .map(|path| path.key())
                .map_err(|_| MutationError::Changed)
        })
        .collect::<Result<HashSet<_>, _>>()?;
    let mut stale = BTreeMap::new();
    for old in before.entries() {
        let Some(new) = after.find(old.canonical_id()) else {
            continue;
        };
        if old.kind() == ContentKind::Modpack
            || (old.kind() == new.kind() && old.managed_filename() == new.managed_filename())
        {
            continue;
        }
        let path = entry_path(old)?;
        let alternate = path
            .strip_suffix(".disabled")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{path}.disabled"));
        for path in [path, alternate] {
            let key = ScopedPath::new_exact(&path)
                .map_err(|_| MutationError::Changed)?
                .key();
            if !protected.contains(&key) {
                stale.insert(path, Proof::entry(old)?);
            }
        }
    }
    Ok(stale)
}

fn preflight(game: &ScopedDirectory, receipt: &Receipt, resume: bool) -> Result<(), MutationError> {
    let manifest = read_path(game, MANIFEST_FILE)?;
    if manifest != receipt.before_manifest
        && (!resume || manifest.as_deref() != Some(receipt.after_manifest.as_slice()))
    {
        return Err(MutationError::Changed);
    }
    // Removing stale provenance has no file mutation, but its absence remains
    // an exact read precondition. A reappearing enabled or disabled file must
    // not disappear from ownership metadata under a successful remove result.
    for path in removed_absence_preconditions(receipt)? {
        if observe_path(game, &path)?.is_some() {
            return Err(MutationError::Conflict);
        }
    }
    for change in &receipt.changes {
        let current = observe_path(game, &change.path)?;
        if current != change.before && (!resume || current != change.after) {
            return Err(MutationError::Conflict);
        }
        if receipt.pack.is_some() && !direct_managed_path(&change.path) {
            continue;
        }
        let alternate = change
            .path
            .strip_suffix(".disabled")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}.disabled", change.path));
        if !receipt
            .changes
            .iter()
            .any(|change| change.path == alternate)
            && observe_path(game, &alternate)?.is_some()
        {
            return Err(MutationError::Conflict);
        }
    }
    Ok(())
}

fn direct_managed_path(path: &str) -> bool {
    path.split_once('/').is_some_and(|(parent, name)| {
        matches!(parent, "mods" | "resourcepacks" | "shaderpacks") && !name.contains('/')
    })
}

fn removed_absence_preconditions(receipt: &Receipt) -> Result<Vec<String>, MutationError> {
    let before = ContentManifest::decode_managed(Some(&receipt.before_observed_manifest))?;
    let after = ContentManifest::decode_managed(Some(&receipt.after_manifest))?;
    if let Some(pack) = &receipt.pack {
        return Ok(pack_stale_paths(&before, &after, pack)?
            .into_keys()
            .filter(|path| !receipt.changes.iter().any(|change| &change.path == path))
            .collect());
    }
    let before = manifest_paths(&before)?;
    let after = manifest_paths(&after)?;
    let mut paths = Vec::new();
    for path in before.keys() {
        if !after.contains_key(path) && !receipt.changes.iter().any(|change| &change.path == path) {
            paths.push(path.clone());
            paths.push(
                path.strip_suffix(".disabled")
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{path}.disabled")),
            );
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

struct ContentAuthority {
    _instance: RegisteredInstance,
    tree: ManagedTreeRoot,
}

fn transaction_root(
    instance: &RegisteredInstance,
    pack: bool,
) -> Result<ManagedContentTransactionRoot, MutationError> {
    let directory = instance.game_directory().capability();
    let effects = directory
        .create_effect_owner()
        .map_err(|_| MutationError::Files)?;
    let tree = ManagedTreeRoot::from_directory(directory.clone(), effects)
        .map_err(|_| MutationError::Files)?;
    let authority = Arc::new(ContentAuthority {
        _instance: instance.clone(),
        tree,
    });
    let operation = authority
        .tree
        .try_acquire()
        .map_err(|_| MutationError::Files)?;
    let root = ManagedContentTransactionRoot::bind(
        operation.directory().map_err(|_| MutationError::Files)?,
        ManagedTransferAuthority::retain(authority),
    );
    Ok(if pack { root.for_pack() } else { root })
}

fn observed(proof: Option<&Proof>) -> ManagedContentObservedState {
    match proof {
        Some(proof) => ManagedContentObservedState::Exact {
            size: proof.size,
            sha512: proof.sha512.clone().into_boxed_str(),
        },
        None => ManagedContentObservedState::Absent,
    }
}

async fn transaction_outcome(
    mut outcome: ManagedContentTransactionOutcome,
    effects: &mut Vec<Effect>,
    owner: &ContentMutations,
    receipt: &mut Receipt,
) -> Result<(), MutationError> {
    const DELAYS_MS: [u64; 4] = [25, 100, 250, 1_000];
    let mut retry = 0;
    let mut current = receipt.clone();
    loop {
        match outcome {
            ManagedContentTransactionOutcome::Committed(_) => {
                *receipt = current;
                return Ok(());
            }
            ManagedContentTransactionOutcome::Cancelled(_) => {
                *receipt = current;
                effects.push(Effect::RolledBack);
                return Err(MutationError::Cancelled);
            }
            ManagedContentTransactionOutcome::Failed(_) => {
                *receipt = current;
                effects.push(Effect::RolledBack);
                return Err(MutationError::Files);
            }
            ManagedContentTransactionOutcome::RecoveryRequired(recovery) => {
                tokio::time::sleep(std::time::Duration::from_millis(DELAYS_MS[retry])).await;
                retry = (retry + 1).min(DELAYS_MS.len() - 1);
                // Cancellation cannot release native effects; the accepted task
                // retains instance admission until this exact recovery settles.
                let owner = owner.clone();
                (outcome, current) = tokio::task::spawn_blocking(move || {
                    let outcome = owner.reconcile_recovery(&mut current, recovery);
                    (outcome, current)
                })
                .await
                .unwrap_or_else(|error| {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                    panic!("content recovery worker stopped before settlement");
                });
            }
        }
    }
}

/// A cancelled native transaction can be a successful compensation for a
/// failed transfer. Preserve that original failure instead of telling the user
/// they cancelled an operation which actually lacked a platform primitive.
async fn unwind_outcome(
    outcome: ManagedContentTransactionOutcome,
    effects: &mut Vec<Effect>,
    failure: MutationError,
    owner: &ContentMutations,
    receipt: &mut Receipt,
) -> Result<(), MutationError> {
    let _ = transaction_outcome(outcome, effects, owner, receipt).await;
    Err(failure)
}

/// The existing leaf owns private staging, authenticated stream copies,
/// displacement, compensation, and exact cleanup. This adapter only supplies
/// the domain intent and keeps the registered instance alive with every leaf.
async fn apply_streamed(
    owner: &ContentMutations,
    instance: &RegisteredInstance,
    receipt: &mut Receipt,
    payloads: &mut HashMap<String, Vec<u8>>,
    pack: Option<&PackPlan>,
    effects: &mut Vec<Effect>,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(DownloadProgress) + Send + Sync)>,
) -> Result<(), MutationError> {
    // Before native preparation every refusal has no payload effects. Once
    // prepared, all exits below consume the native outcome or retain recovery.
    effects.push(Effect::RolledBack);
    let checkpoint_binding = checkpoint_binding(receipt)?;
    let root = transaction_root(instance, receipt.pack.is_some())?;
    let planning = root
        .observe_manifest()
        .map_err(|_| MutationError::Changed)?;
    if planning.manifest_bytes() != receipt.before_manifest.as_deref() {
        return Err(MutationError::Changed);
    }
    let paths = receipt
        .changes
        .iter()
        .map(|change| {
            axial_minecraft::portable_path::PortableRelativePath::new_exact(&change.path)
                .map_err(|_| MutationError::Changed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut observed_paths = paths.clone();
    let absent_paths = removed_absence_preconditions(receipt)?;
    for path in &absent_paths {
        let path = axial_minecraft::portable_path::PortableRelativePath::new_exact(path)
            .map_err(|_| MutationError::Changed)?;
        if !observed_paths.contains(&path) {
            observed_paths.push(path);
        }
    }
    // Empty observation batches are refused by the leaf. Manifest-only
    // transactions still finish its original exact manifest observation, and
    // absent removed paths remain read-preconditions rather than fake writes.
    let planning = if observed_paths.is_empty() {
        planning
    } else {
        planning
            .observe_more(observed_paths)
            .map_err(|_| MutationError::Changed)?
    };
    if planning.observations().iter().any(|observation| {
        absent_paths
            .iter()
            .any(|path| path == observation.path().as_str())
            && observation.state() != &ManagedContentObservedState::Absent
    }) {
        return Err(MutationError::Changed);
    }
    let session = planning
        .finish(paths.clone())
        .map_err(|_| MutationError::Changed)?;
    let observations = session.observations();
    let manifest = session
        .bind_encoded_manifest(receipt.after_manifest.clone())
        .map_err(|_| MutationError::Changed)?;
    let mut mutations = Vec::new();
    let mut plans = Vec::new();
    let mut bodies = HashMap::new();
    let mut downloads = HashMap::new();
    let mut override_paths = HashMap::new();
    let mut payload_sizes = HashMap::new();
    for (index, (change, path)) in receipt.changes.iter().zip(paths).enumerate() {
        let result = if let Some(after) = &change.after {
            let id = ManagedContentPayloadId::new(&format!("payload-{index}"))
                .map_err(|_| MutationError::Changed)?;
            payload_sizes.insert(id.as_str().to_owned(), after.size);
            let sha1 = match change.source.as_ref() {
                Some(Source::Download { file } | Source::PackDownload { file }) => {
                    file.sha1.as_deref()
                }
                _ => None,
            };
            let digest = ExpectedTransferDigests::from_hex(sha1, Some(&after.sha512))
                .map_err(|_| MutationError::Integrity)?;
            let contract = match std::num::NonZeroU64::new(after.size) {
                Some(size) => TransferContract::authenticated_exact(size, digest),
                None => TransferContract::authenticated_below(
                    std::num::NonZeroU64::new(1).expect("positive limit"),
                    digest,
                ),
            }
            .map_err(|_| MutationError::Integrity)?;
            if let Some(bytes) = payloads.remove(&change.path) {
                plans.push(ManagedContentPayloadPlan::from_external_source(
                    id.clone(),
                    contract,
                ));
                bodies.insert(id.as_str().to_owned(), bytes);
            } else {
                match change.source.as_ref().ok_or(MutationError::Changed)? {
                    Source::Download { file } | Source::PackDownload { file } => {
                        plans.push(ManagedContentPayloadPlan::new(id.clone(), contract));
                        downloads.insert(id.as_str().to_owned(), file.url.clone());
                    }
                    Source::Local { path, .. } => {
                        plans.push(ManagedContentPayloadPlan::from_observation(
                            id.clone(),
                            contract,
                            axial_minecraft::portable_path::PortableRelativePath::new_exact(path)
                                .map_err(|_| MutationError::Changed)?,
                        ))
                    }
                    Source::PackOverride => {
                        plans.push(ManagedContentPayloadPlan::from_external_source(
                            id.clone(),
                            contract,
                        ));
                        override_paths.insert(id.as_str().to_owned(), change.path.clone());
                    }
                }
            }
            ManagedContentPathResult::Download(id)
        } else {
            ManagedContentPathResult::Absent
        };
        mutations.push(ManagedContentPathMutation::new(
            path,
            observed(change.before.as_ref()),
            result,
        ));
    }
    let plan = ManagedContentMutationPlan::new(&observations, mutations, plans, manifest)
        .map_err(|_| MutationError::Changed)?;
    let payload_count = i32::try_from(payload_sizes.len()).map_err(|_| MutationError::Capacity)?;
    let bytes_total = payload_sizes.values().try_fold(0_u64, |total, size| {
        total.checked_add(*size).ok_or(MutationError::Capacity)
    })?;
    let mut completed = 0;
    let mut bytes_done = 0;
    let mut transfers = match session.prepare(plan) {
        ManagedContentPreparationOutcome::Prepared(prepared) => prepared.into_transfer_batch(),
        ManagedContentPreparationOutcome::Refused { .. } => {
            return Err(MutationError::Changed);
        }
        ManagedContentPreparationOutcome::RecoveryRequired(effect) => {
            return transaction_outcome(
                ManagedContentTransactionOutcome::RecoveryRequired(effect),
                effects,
                owner,
                receipt,
            )
            .await;
        }
    };
    loop {
        if let Err(error) =
            owner.persist_checkpoint(receipt, transfers.checkpoint(checkpoint_binding))
        {
            return unwind_outcome(transfers.cancel(), effects, error, owner, receipt).await;
        }
        if completed > 0 {
            report_progress(
                progress,
                "content_download",
                completed,
                payload_count,
                Some((bytes_done, bytes_total)),
            );
        }
        match transfers.next() {
            ManagedContentTransferStep::Issued(issued) => {
                if cancel.is_cancelled() {
                    return transaction_outcome(issued.cancel(), effects, owner, receipt).await;
                }
                let Some(size) = payload_sizes.remove(issued.id().as_str()) else {
                    return unwind_outcome(
                        issued.cancel(),
                        effects,
                        MutationError::Changed,
                        owner,
                        receipt,
                    )
                    .await;
                };
                if completed == 0 {
                    report_progress(
                        progress,
                        "content_download",
                        0,
                        payload_count,
                        Some((0, bytes_total)),
                    );
                }
                let (cancel_sender, cancellation) = transfer_cancellation_channel();
                let settlement = if issued.is_external() {
                    let bytes = match bodies.remove(issued.id().as_str()) {
                        Some(bytes) => Ok(bytes),
                        None => override_paths
                            .remove(issued.id().as_str())
                            .and_then(|path| pack.map(|pack| pack.override_bytes(&path)))
                            .ok_or(MutationError::Changed)
                            .and_then(|result| result.map_err(MutationError::from)),
                    };
                    let bytes = match bytes {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            return unwind_outcome(issued.cancel(), effects, error, owner, receipt)
                                .await;
                        }
                    };
                    match issued.copy_external(io::Cursor::new(bytes), cancellation) {
                        Ok(settlement) => settlement,
                        Err(issued) => {
                            return unwind_outcome(
                                issued.cancel(),
                                effects,
                                MutationError::Files,
                                owner,
                                receipt,
                            )
                            .await;
                        }
                    }
                } else if issued.is_local() {
                    issued.copy_local(cancellation)
                } else {
                    let Some(raw_url) = downloads.remove(issued.id().as_str()) else {
                        return unwind_outcome(
                            issued.cancel(),
                            effects,
                            MutationError::Changed,
                            owner,
                            receipt,
                        )
                        .await;
                    };
                    let transfer = async {
                        #[cfg(test)]
                        if let Some((url, client)) = &owner.transfer_fixture {
                            if raw_url == url.as_str() {
                                return Some((client.clone(), url.clone()));
                            }
                        }
                        let url = validate_download_url(&raw_url).ok()?;
                        let origin = TransferOrigin::from_url(&url).ok()?;
                        let client = crate::network::pinned_public_transfer_client(origin, &url)
                            .await
                            .ok()?;
                        Some((client, url))
                    }
                    .await;
                    let Some((client, url)) = transfer else {
                        return unwind_outcome(
                            issued.cancel(),
                            effects,
                            MutationError::Unavailable,
                            owner,
                            receipt,
                        )
                        .await;
                    };
                    let running = match issued.start(
                        client,
                        url,
                        crate::network::managed_transfer_retry_policy(),
                        cancellation,
                    ) {
                        Ok(running) => running,
                        Err(issued) => {
                            return unwind_outcome(
                                issued.cancel(),
                                effects,
                                MutationError::Files,
                                owner,
                                receipt,
                            )
                            .await;
                        }
                    };
                    let joined = running.join();
                    tokio::pin!(joined);
                    tokio::select! {
                        settlement = &mut joined => settlement,
                        _ = cancel.cancelled() => { cancel_sender.cancel(); joined.await },
                    }
                };
                let failure = match settlement.failure_report().map(|report| report.last()) {
                    Some(TransferFailureKind::Cancelled) => MutationError::Cancelled,
                    Some(TransferFailureKind::StageCreate(io::ErrorKind::Unsupported)) => {
                        MutationError::StreamingUnsupported
                    }
                    Some(
                        TransferFailureKind::DigestMismatch(_)
                        | TransferFailureKind::SizeMismatch { .. }
                        | TransferFailureKind::ByteLimitExceeded { .. },
                    ) => MutationError::Integrity,
                    _ => MutationError::Files,
                };
                transfers = match settlement.advance() {
                    ManagedContentTransferAdvance::Continue(transfers) => transfers,
                    ManagedContentTransferAdvance::Unwind(outcome) => {
                        return unwind_outcome(outcome, effects, failure, owner, receipt).await;
                    }
                };
                completed += 1;
                bytes_done += size;
            }
            ManagedContentTransferStep::Complete(complete) => {
                return match complete.stage() {
                    ManagedContentStageOutcome::Ready(ready) => {
                        if cancel.is_cancelled() {
                            return transaction_outcome(ready.cancel(), effects, owner, receipt)
                                .await;
                        }
                        let mut ready = match ready.prepare_publication(checkpoint_binding) {
                            ManagedContentStageOutcome::Ready(ready) => ready,
                            ManagedContentStageOutcome::Unwind(outcome) => {
                                let failure = if matches!(
                                    outcome,
                                    ManagedContentTransactionOutcome::Failed(
                                        ManagedContentTransactionFailure::ObservationDrift
                                    )
                                ) {
                                    MutationError::Changed
                                } else {
                                    MutationError::Files
                                };
                                return unwind_outcome(outcome, effects, failure, owner, receipt)
                                    .await;
                            }
                        };
                        if let Err(error) = owner.persist_checkpoint(
                            receipt,
                            ready.checkpoint(checkpoint_binding).map(Some),
                        ) {
                            return unwind_outcome(ready.cancel(), effects, error, owner, receipt)
                                .await;
                        }
                        report_progress(progress, "content_commit", 0, 1, None);
                        if cancel.is_cancelled() {
                            return transaction_outcome(ready.cancel(), effects, owner, receipt)
                                .await;
                        }
                        let mut checkpoint_error = None;
                        let budget = checkpoint_budget(receipt).unwrap_or(0);
                        let outcome =
                            ready.commit_with_checkpoint(checkpoint_binding, budget, |proof| {
                                match owner.persist_checkpoint(receipt, Ok(Some(proof))) {
                                    Ok(stored) => Ok(stored),
                                    Err(error) => {
                                        checkpoint_error = Some(error);
                                        Err(())
                                    }
                                }
                            });
                        match checkpoint_error {
                            Some(error) => {
                                unwind_outcome(outcome, effects, error, owner, receipt).await
                            }
                            None => transaction_outcome(outcome, effects, owner, receipt).await,
                        }
                    }
                    ManagedContentStageOutcome::Unwind(outcome) => {
                        unwind_outcome(outcome, effects, MutationError::Files, owner, receipt).await
                    }
                };
            }
        }
    }
}

fn report_progress(
    callback: Option<&(dyn Fn(DownloadProgress) + Send + Sync)>,
    phase: &str,
    current: i32,
    total: i32,
    bytes: Option<(u64, u64)>,
) {
    if let Some(callback) = callback {
        callback(DownloadProgress {
            phase: phase.into(),
            current,
            total,
            file: None,
            error: None,
            done: false,
            bytes_done: bytes.map(|(done, _)| done),
            bytes_total: bytes.map(|(_, total)| total),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        instances::{
            create::{CreateInstanceRequest, CreateTarget, InstanceService},
            directory::Registry,
        },
        library::{LibraryLifecycle, LibraryOpenOutcome},
        network::ClientConfig,
        tasks::Exclusions,
    };
    use std::io::Write;

    fn catalog(owner: &ContentMutations) -> ContentService {
        ContentService::new(owner._client.clone()).unwrap()
    }

    async fn identity_provider(
        hash: &str,
        title_response: bool,
    ) -> (ContentService, tokio::task::JoinHandle<()>) {
        let mut responses = vec![serde_json::json!({ hash: {
            "project_id": "known", "id": "known-v1", "name": "Identified fixture", "version_number": "1",
            "version_type": "release", "loaders": [], "game_versions": ["1.21.4"],
            "files": [{"filename":"known.zip","url":"https://cdn.example.com/known.zip","size":5,
                "primary":true,"hashes":{"sha512":hash}}], "dependencies": [],
        }}).to_string()];
        if title_response {
            responses.push("malformed title metadata".into());
        }
        metadata_provider(responses).await
    }

    async fn metadata_provider(
        responses: Vec<String>,
    ) -> (ContentService, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let service = ContentService::with_base_url(
            ProviderClient::new(ClientConfig::default()).unwrap(),
            &origin,
            crate::network::OriginPolicy::loopback_for_tests([&origin], 0).unwrap(),
        )
        .unwrap();
        let task = tokio::spawn(async move {
            for body in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() <= 64 * 1024);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        (service, task)
    }

    fn pack(indexed: &[(&str, &[u8])], overrides: &[(&str, &[u8])]) -> ResolvedPack {
        let files = indexed.iter().map(|(path, bytes)| serde_json::json!({
            "path": path, "fileSize": bytes.len(), "hashes": {"sha512": Proof::bytes(bytes).sha512},
            "downloads": [format!("https://cdn.example.com/{}", path.rsplit('/').next().unwrap())],
        })).collect::<Vec<_>>();
        pack_with_index(
            serde_json::json!({
                "name":"Pack fixture", "dependencies":{"minecraft":"1.21.4"}, "files":files,
            }),
            overrides,
        )
    }

    fn prefix_pack() -> ResolvedPack {
        let paths = (0..513)
            .map(|index| format!("overrides/config/prefix-{index:04}.txt"))
            .collect::<Vec<_>>();
        let overrides = paths
            .iter()
            .map(|path| (path.as_str(), b"new payload".as_slice()))
            .collect::<Vec<_>>();
        pack(&[], &overrides)
    }

    fn pack_with_index(index: serde_json::Value, overrides: &[(&str, &[u8])]) -> ResolvedPack {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "modrinth.index.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer
            .write_all(&serde_json::to_vec(&index).unwrap())
            .unwrap();
        for (path, bytes) in overrides {
            writer
                .start_file(*path, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        ResolvedPack {
            canonical_id: CanonicalId::for_project(ProviderId::Modrinth, "fixture-pack"),
            version_id: "pack-v1".into(),
            name: "Pack fixture".into(),
            archive: super::super::packs::PackArchive::read(writer.finish().unwrap().into_inner())
                .unwrap(),
        }
    }

    /// An exact durable intent for exercising crash/cancellation boundaries
    /// after acceptance, before the native transaction completes.
    fn pack_record(instance: &RegisteredInstance, pack: &ResolvedPack) -> Receipt {
        let plan = pack.archive.plan_all(true).unwrap();
        let mut files = Vec::new();
        let mut changes = Vec::new();
        for file in plan.files() {
            let proof = Proof {
                size: file.size.unwrap(),
                sha512: file.sha512.clone().unwrap(),
            };
            files.push(PackInstalledFile {
                path: file.path.clone(),
                size: proof.size,
                sha512: proof.sha512.clone(),
            });
            changes.push(Change {
                path: file.path.clone(),
                before: None,
                after: Some(proof),
                source: Some(Source::PackDownload {
                    file: FileRef {
                        filename: file.filename().into(),
                        url: file.url.clone(),
                        size: file.size,
                        sha512: file.sha512.clone(),
                        sha1: file.sha1.clone(),
                        primary: true,
                    },
                }),
            });
        }
        for (path, size, sha512) in plan.overrides() {
            files.push(PackInstalledFile {
                path: path.into(),
                size,
                sha512: sha512.into(),
            });
            changes.push(Change {
                path: path.into(),
                before: None,
                after: Some(Proof {
                    size,
                    sha512: sha512.into(),
                }),
                source: Some(Source::PackOverride),
            });
        }
        let mut entry = ManifestEntry::provenance(
            pack.canonical_id.clone(),
            ProviderId::Modrinth,
            pack.canonical_id.project_id().into(),
            pack.version_id.clone(),
            Some(pack.name.clone()),
        )
        .unwrap();
        entry
            .record_pack_installation(PackInstallation {
                fingerprint: plan.fingerprint().into(),
                files,
            })
            .unwrap();
        let (before, _, raw) = observe(instance.game_directory()).unwrap();
        let mut after = before.clone();
        after.try_upsert(entry).unwrap();
        let receipt = Receipt {
            schema: 1,
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.record().instance.id.clone(),
            instance_revision: instance.record().revision,
            directory_receipt: instance.game_directory().receipt().unwrap(),
            before_manifest: raw,
            before_observed_manifest: before.encode_managed().unwrap(),
            after_manifest: after.encode_managed().unwrap(),
            changes,
            native_settled: false,
            pack: Some(pack.canonical_id.clone()),
            local_mod: None,
            ready_checkpoint: None,
        };
        validate_receipt(&receipt).unwrap();
        receipt
    }

    fn open_crash_fixture(
        root: &std::path::Path,
        library_id: crate::library::LibraryId,
    ) -> ContentMutations {
        let library = match LibraryLifecycle::open_with_id(root, library_id) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("content crash fixture root: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::open(root.join("metadata.sqlite")).unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::delete::MIGRATION,
                MIGRATION,
                crate::performance::mutation::MIGRATION,
                crate::performance::rules::MIGRATION,
            ])
            .unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let tasks = TaskOwner::new(8).unwrap();
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let performance = crate::performance::PerformanceService::new(
            storage,
            directories.clone(),
            tasks.clone(),
            Arc::new(ContentService::new(client.clone()).unwrap()),
            crate::performance::public_transfer_resolver(),
        )
        .unwrap();
        ContentMutations::new(directories, client, tasks).with_performance(performance)
    }

    async fn fixture() -> (tempfile::TempDir, ContentMutations, InstanceId) {
        let root = tempfile::tempdir().unwrap();
        let library = match LibraryLifecycle::open(&root.path().canonicalize().unwrap()) {
            LibraryOpenOutcome::Ready(library) => library,
            other => panic!("fixture root: {other:?}"),
        };
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage
            .migrate(&[
                crate::instances::directory::MIGRATION,
                crate::instances::create::MIGRATION,
                crate::instances::delete::MIGRATION,
                MIGRATION,
                crate::performance::mutation::MIGRATION,
                crate::performance::rules::MIGRATION,
            ])
            .unwrap();
        let directories =
            InstanceDirectories::new(Registry::new(storage.clone()), library, Exclusions::new());
        let tasks = TaskOwner::new(16).unwrap();
        let instances = InstanceService::new(directories.clone(), tasks.clone());
        let instance = instances
            .create(
                CreateInstanceRequest {
                    name: "Content fixture".into(),
                    selection_id: "vanilla|1.21.4".into(),
                    ..Default::default()
                },
                CreateTarget {
                    selection_id: "vanilla|1.21.4".into(),
                    version_id: "1.21.4".into(),
                    minecraft_version: "1.21.4".into(),
                    loader_key: "vanilla".into(),
                },
            )
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let client = ProviderClient::new(ClientConfig::default()).unwrap();
        let performance = crate::performance::PerformanceService::new(
            storage,
            directories.clone(),
            tasks.clone(),
            Arc::new(ContentService::new(client.clone()).unwrap()),
            crate::performance::public_transfer_resolver(),
        )
        .unwrap();
        let owner = ContentMutations::new(directories, client, tasks).with_performance(performance);
        (root, owner, instance.id)
    }

    fn entry(bytes: &[u8]) -> ManifestEntry {
        ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "pack"),
            ProviderId::Modrinth,
            "pack".into(),
            "v1".into(),
            ContentKind::ResourcePack,
            &FileRef {
                url: "https://example.invalid/pack.zip".into(),
                filename: "pack.zip".into(),
                sha512: Some(Proof::bytes(bytes).sha512),
                sha1: None,
                size: Some(bytes.len() as u64),
                primary: true,
            },
            Vec::new(),
            None,
        )
        .unwrap()
    }

    fn receipt(owner: &ContentMutations, instance: &RegisteredInstance, bytes: &[u8]) -> Receipt {
        let (before, _, raw) = observe(instance.game_directory()).unwrap();
        let mut after = before.clone();
        after.try_upsert(entry(bytes)).unwrap();
        let path = "resourcepacks/pack.zip".to_string();
        let receipt = Receipt {
            schema: 1,
            operation_id: uuid::Uuid::new_v4().to_string(),
            instance_id: instance.record().instance.id.clone(),
            instance_revision: instance.record().revision,
            directory_receipt: instance.game_directory().receipt().unwrap(),
            before_manifest: raw,
            before_observed_manifest: before.encode_managed().unwrap(),
            after_manifest: after.encode_managed().unwrap(),
            native_settled: false,
            pack: None,
            local_mod: None,
            ready_checkpoint: None,
            changes: vec![Change {
                path,
                before: before
                    .find(&CanonicalId("modrinth:pack".into()))
                    .map(|entry| Proof::entry(entry).unwrap()),
                after: Some(Proof::bytes(bytes)),
                source: Some(Source::Download {
                    file: FileRef {
                        url: "https://example.invalid/pack.zip".into(),
                        filename: "pack.zip".into(),
                        sha512: Some(Proof::bytes(bytes).sha512),
                        sha1: None,
                        size: Some(bytes.len() as u64),
                        primary: true,
                    },
                }),
            }],
        };
        validate_receipt(&receipt).unwrap();
        owner
            .directories
            .registry()
            .storage()
            .transaction(|tx| -> Result<(), MutationError> {
                tx.execute(
                    "INSERT INTO content_batches VALUES(?1,?2,?3)",
                    params![
                        receipt.instance_id.as_str(),
                        receipt.operation_id,
                        serde_json::to_string(&receipt).unwrap()
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        receipt
    }

    #[tokio::test]
    async fn content_reads_during_launch_preserve_exclusive_mutations() {
        let (root, owner, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(entry(b"owned")).unwrap();
        let raw = manifest.encode_managed().unwrap();
        std::fs::write(game.join(MANIFEST_FILE), &raw).unwrap();
        std::fs::write(game.join("resourcepacks/pack.zip"), b"owned").unwrap();
        let launch = owner.directories.admit(&id).unwrap();
        let read = owner.directories.admit_read(&id).unwrap();
        launch
            .record_successful_launch("2026-09-27T10:00:00Z")
            .unwrap();
        read.validate_current().unwrap();
        assert_eq!(owner.installed(&id).unwrap(), manifest);
        let (_, observed, live) = owner.instance_context(&id).unwrap();
        assert_eq!(observed, manifest);
        assert!(live.contains(&manifest.entries()[0]));
        assert!(matches!(
            owner.remove(&id, manifest.entries()[0].canonical_id()),
            Err(MutationError::Unavailable)
        ));
        assert!(matches!(
            owner.plan(&catalog(&owner), &id, &[]).await,
            Err(MutationError::Unavailable)
        ));
        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), raw);
        assert_eq!(
            std::fs::read(game.join("resourcepacks/pack.zip")).unwrap(),
            b"owned"
        );
    }

    fn local_mod_record(owner: &ContentMutations, instance: &RegisteredInstance) -> Receipt {
        let game = instance.game_directory().read_projection().unwrap();
        let mut manifest = ContentManifest::default();
        manifest
            .try_upsert(
                ManifestEntry::managed_file(
                    CanonicalId::for_project(ProviderId::Modrinth, "recorded"),
                    ProviderId::Modrinth,
                    "recorded".into(),
                    "v1".into(),
                    ContentKind::Mod,
                    ManagedContentFileName::new_exact("recorded.jar").unwrap(),
                    Some(Proof::bytes(b"managed").sha512),
                    Some(7),
                    vec![],
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        let mut raw = b" \n".to_vec();
        raw.extend(manifest.encode_managed().unwrap());
        raw.extend_from_slice(b"\n ");
        std::fs::write(game.join(MANIFEST_FILE), raw).unwrap();
        std::fs::write(game.join("mods/recorded.jar"), b"local replacement").unwrap();
        owner
            .plan_local_mod_change(
                instance,
                &portable("recorded.jar").unwrap(),
                Some(&portable("recorded.jar.disabled").unwrap()),
            )
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn local_mod_receipt_refuses_later_source_manifest_and_destination_changes() {
        for change in ["source-bytes", "source-identity", "manifest", "destination"] {
            let (root, owner, id) = fixture().await;
            let game = root.path().join("instances").join(id.as_str());
            let instance = owner.directories.admit(&id).unwrap();
            let record = local_mod_record(&owner, &instance);
            let source = game.join("mods/recorded.jar");
            let destination = game.join("mods/recorded.jar.disabled");
            let mut expected_manifest = record.after_manifest.clone();
            match change {
                "source-bytes" => std::fs::write(&source, b"later bytes").unwrap(),
                "source-identity" => {
                    std::fs::rename(&source, game.join("mods/preserved.jar")).unwrap();
                    std::fs::write(&source, b"later bytes").unwrap();
                }
                "manifest" => {
                    expected_manifest.push(b' ');
                    std::fs::write(game.join(MANIFEST_FILE), &expected_manifest).unwrap();
                }
                "destination" => std::fs::write(&destination, b"user destination").unwrap(),
                _ => unreachable!(),
            }
            assert!(
                owner
                    .accept_batch(instance, record, &CancellationToken::new())
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(
                std::fs::read(&source).unwrap(),
                if change.starts_with("source-") {
                    b"later bytes".as_slice()
                } else {
                    b"local replacement".as_slice()
                }
            );
            if change == "destination" {
                assert_eq!(std::fs::read(&destination).unwrap(), b"user destination");
            } else {
                assert!(!destination.exists());
            }
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                expected_manifest
            );
            assert!(!owner.has_unsettled_effects());
            assert!(owner.directories.admit(&id).is_ok());
        }
    }

    #[tokio::test]
    async fn local_mod_receipt_rejects_unrelated_or_managed_mutation_authority() {
        let (_root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let original = local_mod_record(&owner, &instance);
        for tamper in [
            "source",
            "destination",
            "managed",
            "download",
            "extra",
            "manifest",
            "pack",
            "digest",
        ] {
            let mut record = original.clone();
            match tamper {
                "source" => record.local_mod.as_mut().unwrap().source = "mods/unrelated.jar".into(),
                "destination" => {
                    record.local_mod.as_mut().unwrap().destination =
                        Some("mods/unrelated.jar".into())
                }
                "managed" => {
                    let managed = Proof::bytes(b"managed");
                    record.changes[1].before = Some(managed.clone());
                    record.changes[0].after = Some(managed.clone());
                    record.changes[0].source = Some(Source::Local {
                        path: "mods/recorded.jar".into(),
                        proof: managed,
                    });
                }
                "download" => record.changes[0].source = Some(Source::PackOverride),
                "extra" => record.changes.push(record.changes[1].clone()),
                "manifest" => record.after_manifest.push(b' '),
                "pack" => {
                    record.pack = Some(CanonicalId::for_project(ProviderId::Modrinth, "pack"))
                }
                "digest" => record.changes[1].before.as_mut().unwrap().sha512 = "invalid".into(),
                _ => unreachable!(),
            }
            assert!(validate_receipt(&record).is_err(), "{tamper}");
        }
        let encoded = serde_json::to_string(&original).unwrap();
        validate_receipt(&serde_json::from_str(&encoded).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn local_mod_receipt_must_be_durably_inserted_unchanged_before_file_effects() {
        for trigger in [
            "CREATE TRIGGER refuse_content_receipt BEFORE INSERT ON content_batches BEGIN SELECT RAISE(IGNORE); END;",
            "CREATE TRIGGER change_content_receipt AFTER INSERT ON content_batches BEGIN UPDATE content_batches SET receipt_json='{}' WHERE instance_id=NEW.instance_id; END;",
        ] {
            let (root, owner, id) = fixture().await;
            let instance = owner.directories.admit(&id).unwrap();
            let record = local_mod_record(&owner, &instance);
            owner
                .directories
                .registry()
                .storage()
                .transaction(|tx| tx.execute_batch(trigger).map_err(StorageError::from))
                .unwrap();
            assert!(matches!(
                owner
                    .change_local_mod_admitted(
                        instance,
                        &portable("recorded.jar").unwrap(),
                        Some(&portable("recorded.jar.disabled").unwrap()),
                        &CancellationToken::new(),
                    )
                    .await,
                Err(MutationError::Changed)
            ));
            let game = root.path().join("instances").join(id.as_str());
            assert_eq!(
                std::fs::read(game.join("mods/recorded.jar")).unwrap(),
                b"local replacement"
            );
            assert!(!game.join("mods/recorded.jar.disabled").exists());
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                record.after_manifest
            );
            assert!(!owner.has_unsettled_effects());
            assert!(owner.directories.admit(&id).is_ok());
        }
    }

    #[tokio::test]
    async fn local_mod_restart_requires_native_settlement_and_verifies_acknowledged_publication() {
        for phase in ["prepared", "committed", "acknowledged"] {
            let (root, owner, id) = fixture().await;
            let instance = owner.directories.admit(&id).unwrap();
            let mut record = local_mod_record(&owner, &instance);
            owner.persist_receipt(&record).unwrap();
            if phase != "prepared" {
                apply_streamed(
                    &owner,
                    &instance,
                    &mut record,
                    &mut HashMap::new(),
                    None,
                    &mut Vec::new(),
                    &CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
                if phase == "acknowledged" {
                    let raw = encoded_receipt(&record).unwrap();
                    owner.mark_native_settled(&mut record, &raw).unwrap();
                }
            }
            drop(instance);
            let reopened = ContentMutations::new(
                owner.directories.clone(),
                owner._client.clone(),
                owner.tasks.clone(),
            );
            assert!(reopened.directories.admit(&id).is_err());
            let result = reopened.resume(&id).unwrap().join().await.unwrap();
            if phase == "acknowledged" {
                assert_eq!(result.unwrap().status, "complete");
                assert!(reopened.directories.admit(&id).is_ok());
                assert!(!reopened.has_unsettled_effects());
            } else {
                assert!(matches!(result, Err(MutationError::Pending)));
                assert!(reopened.directories.admit(&id).is_err());
                assert!(reopened.has_unsettled_effects());
            }
            let game = root.path().join("instances").join(id.as_str());
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                record.after_manifest
            );
            let file = if phase == "prepared" {
                "mods/recorded.jar"
            } else {
                "mods/recorded.jar.disabled"
            };
            assert_eq!(
                std::fs::read(game.join(file)).unwrap(),
                b"local replacement"
            );
        }
    }

    #[tokio::test]
    async fn cancelled_local_mod_receipt_keeps_files_and_releases_admission() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let record = local_mod_record(&owner, &instance);
        let expected = record.after_manifest.clone();
        owner.persist_receipt(&record).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            owner.apply(instance, record, HashMap::new(), &cancel).await,
            Err(MutationError::Cancelled)
        ));
        assert!(owner.directories.admit(&id).is_ok());
        assert!(!owner.has_unsettled_effects());
        let game = root.path().join("instances").join(id.as_str());
        assert_eq!(
            std::fs::read(game.join("mods/recorded.jar")).unwrap(),
            b"local replacement"
        );
        assert!(!game.join("mods/recorded.jar.disabled").exists());
        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), expected);
    }

    #[tokio::test]
    async fn content_read_target_identity_must_remain_current() {
        let (_root, owner, id) = fixture().await;
        let registry = owner.directories.registry();
        let original = serde_json::to_value(registry.get_live(&id).unwrap()).unwrap();
        for (field, replacement) in [
            ("version_id", "1.21.5"),
            ("loader_key", "fabric"),
            ("minecraft_version", "1.21.5"),
        ] {
            let read = owner.directories.admit_read(&id).unwrap();
            observe(read.game_directory()).unwrap();
            let mut changed = original.clone();
            changed["instance"][field] = replacement.into();
            registry
                .storage()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE instances SET record_json=?2 WHERE id=?1",
                        params![id.as_str(), changed.to_string()],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(
                read.validate_current(),
                Err(crate::instances::model::InstanceError::Conflict)
            ));
            registry
                .storage()
                .transaction(|tx| -> Result<(), StorageError> {
                    tx.execute(
                        "UPDATE instances SET record_json=?2 WHERE id=?1",
                        params![id.as_str(), original.to_string()],
                    )?;
                    Ok(())
                })
                .unwrap();
            read.validate_current().unwrap();
        }
    }

    #[tokio::test]
    async fn content_reads_reject_pending_effects_and_replaced_directory_binding() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let read = owner.directories.admit_read(&id).unwrap();
        let pending = receipt(&owner, &instance, b"owned");
        assert!(read.validate_current().is_err());
        assert!(owner.installed(&id).is_err());
        assert!(owner.instance_context(&id).is_err());
        owner
            .directories
            .registry()
            .storage()
            .transaction(|tx| -> Result<(), StorageError> {
                tx.execute(
                    "DELETE FROM content_batches WHERE operation_id=?1",
                    [&pending.operation_id],
                )?;
                Ok(())
            })
            .unwrap();
        read.validate_current().unwrap();
        let game = root.path().join("instances").join(id.as_str());
        let preserved = root.path().join("preserved-instance");
        std::fs::rename(&game, &preserved).unwrap();
        std::fs::create_dir(&game).unwrap();
        assert!(read.validate_current().is_err());
        assert!(owner.installed(&id).is_err());
        assert!(owner.instance_context(&id).is_err());
        assert!(preserved.join("resourcepacks").is_dir());
        assert!(!game.join(MANIFEST_FILE).exists());
    }

    #[tokio::test]
    async fn approved_content_proof_verifies_live_bytes_and_preserves_enabled_state() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let bytes = b"approved payload";
        let approved = PlannedFile::new(
            CanonicalId::for_project(ProviderId::Modrinth, "pack"),
            ProviderId::Modrinth,
            "pack".into(),
            "v1".into(),
            ContentKind::ResourcePack,
            FileRef {
                filename: "pack.zip".into(),
                url: "https://example.invalid/pack.zip".into(),
                size: Some(bytes.len() as u64),
                sha512: Some(Proof::bytes(bytes).sha512),
                sha1: None,
                primary: true,
            },
            Vec::new(),
            Some("Presentation is not artifact authority".into()),
        )
        .unwrap();
        let encoded = serde_json::to_value(&approved).unwrap();
        let approved: PlannedFile = serde_json::from_value(encoded.clone()).unwrap();
        let mut unknown = encoded;
        unknown["authority"] = serde_json::json!("caller path");
        assert!(serde_json::from_value::<PlannedFile>(unknown).is_err());
        assert!(
            !owner
                .installed_content_admitted(&instance, std::slice::from_ref(&approved))
                .unwrap()
        );
        let record = receipt(&owner, &instance, bytes);
        owner
            .apply(
                instance.clone(),
                record,
                HashMap::from([("resourcepacks/pack.zip".into(), bytes.to_vec())]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            owner
                .installed_content_admitted(&instance, std::slice::from_ref(&approved))
                .unwrap()
        );
        let mut wrong_version = approved.clone();
        wrong_version.version_id = "v2".into();
        assert!(matches!(
            owner.installed_content_admitted(&instance, &[wrong_version]),
            Err(MutationError::Changed)
        ));
        let mut invalid = approved.clone();
        invalid.file.filename = "../escape.zip".into();
        assert!(
            owner
                .installed_content_admitted(&instance, &[invalid])
                .is_err()
        );
        assert!(
            owner
                .installed_content_admitted(&instance, &[approved.clone(), approved.clone()])
                .is_err()
        );

        let directory = root.path().join("instances").join(id.as_str());
        let enabled = directory.join("resourcepacks/pack.zip");
        let disabled = directory.join("resourcepacks/pack.zip.disabled");
        std::fs::rename(&enabled, &disabled).unwrap();
        assert!(
            owner
                .installed_content_admitted(&instance, std::slice::from_ref(&approved))
                .unwrap()
        );
        std::fs::write(&enabled, bytes).unwrap();
        assert!(matches!(
            owner.installed_content_admitted(&instance, std::slice::from_ref(&approved)),
            Err(MutationError::Changed)
        ));
        std::fs::remove_file(&enabled).unwrap();
        std::fs::write(&disabled, b"user edit").unwrap();
        assert!(matches!(
            owner.installed_content_admitted(&instance, std::slice::from_ref(&approved)),
            Err(MutationError::Changed)
        ));
        std::fs::remove_file(&disabled).unwrap();
        assert!(matches!(
            owner.installed_content_admitted(&instance, &[approved]),
            Err(MutationError::Changed)
        ));
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn streamed_transaction_installs_large_payload_updates_and_removes_only_owned_files() {
        let (root, owner, id) = fixture().await;
        let payload = vec![42; 17 * 1024 * 1024];
        let instance = owner.directories.admit(&id).unwrap();
        let record = receipt(&owner, &instance, &payload);
        let result = owner
            .apply(
                instance,
                record,
                HashMap::from([("resourcepacks/pack.zip".into(), payload.clone())]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, "complete");
        let directory = root.path().join("instances").join(id.as_str());
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/pack.zip")).unwrap(),
            payload
        );
        std::fs::write(directory.join("resourcepacks/user.zip"), b"user bytes").unwrap();
        let instance = owner.directories.admit(&id).unwrap();
        let record = receipt(&owner, &instance, b"new payload");
        owner
            .apply(
                instance,
                record,
                HashMap::from([("resourcepacks/pack.zip".into(), b"new payload".to_vec())]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        owner
            .remove(&id, &CanonicalId("modrinth:pack".into()))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(!directory.join("resourcepacks/pack.zip").exists());
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/user.zip")).unwrap(),
            b"user bytes"
        );
        assert!(owner.installed(&id).unwrap().is_empty());
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn cancellation_settles_stages_and_does_not_publish_or_keep_a_false_pending_fence() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let record = receipt(&owner, &instance, b"cancelled payload");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            owner
                .apply(
                    instance,
                    record,
                    HashMap::from([(
                        "resourcepacks/pack.zip".into(),
                        b"cancelled payload".to_vec()
                    )]),
                    &cancel
                )
                .await,
            Err(MutationError::Cancelled)
        ));
        assert!(
            !root
                .path()
                .join("instances")
                .join(id.as_str())
                .join("resourcepacks/pack.zip")
                .exists()
        );
        assert!(!owner.has_unsettled_effects());
        assert!(owner.directories.admit(&id).is_ok());
    }

    #[tokio::test]
    async fn final_bytes_without_a_native_cleanup_receipt_never_clear_a_restart_fence() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let record = receipt(&owner, &instance, b"published bytes");
        let directory = root.path().join("instances").join(id.as_str());
        std::fs::write(directory.join("resourcepacks/pack.zip"), b"published bytes").unwrap();
        std::fs::write(directory.join(MANIFEST_FILE), record.after_manifest).unwrap();
        drop(instance);
        for _ in 0..2 {
            assert!(matches!(
                owner.resume(&id).unwrap().join().await.unwrap(),
                Err(MutationError::Pending)
            ));
            assert!(owner.has_unsettled_effects());
            assert!(owner.directories.admit(&id).is_err());
        }
    }

    #[tokio::test]
    async fn externally_toggled_owned_bytes_are_observed_and_missing_metadata_can_be_removed() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(entry(b"owned bytes")).unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            manifest.encode_managed().unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("resourcepacks/pack.zip.disabled"),
            b"owned bytes",
        )
        .unwrap();
        let (target, observed, live) = owner.instance_context(&id).unwrap();
        assert_eq!(target.game_version, "1.21.4");
        let owned = observed.find(&CanonicalId("modrinth:pack".into())).unwrap();
        assert!(!owned.enabled());
        assert!(live.contains(owned));
        std::fs::remove_file(directory.join("resourcepacks/pack.zip.disabled")).unwrap();
        owner
            .remove(&id, &CanonicalId("modrinth:pack".into()))
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert!(owner.installed(&id).unwrap().is_empty());
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn stale_metadata_removal_preserves_a_reappearing_unowned_disabled_file() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        let mut manifest = ContentManifest::default();
        manifest.try_upsert(entry(b"owned bytes")).unwrap();
        let original = manifest.encode_managed().unwrap();
        std::fs::write(directory.join(MANIFEST_FILE), &original).unwrap();
        std::fs::write(
            directory.join("resourcepacks/pack.zip.disabled"),
            b"new user bytes",
        )
        .unwrap();
        assert!(matches!(
            owner
                .remove(&id, &CanonicalId("modrinth:pack".into()))
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(MutationError::Conflict)
        ));
        assert_eq!(
            std::fs::read(directory.join(MANIFEST_FILE)).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/pack.zip.disabled")).unwrap(),
            b"new user bytes"
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn pack_provenance_only_removal_commits_without_claiming_payload_files() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        let canonical_id = CanonicalId("modrinth:provenance".into());
        let mut manifest = ContentManifest::default();
        manifest
            .try_upsert(
                ManifestEntry::provenance(
                    canonical_id.clone(),
                    ProviderId::Modrinth,
                    "provenance".into(),
                    "v1".into(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            manifest.encode_managed().unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("resourcepacks/user.zip"),
            b"unowned pack bytes",
        )
        .unwrap();
        let result = owner
            .remove(&id, &canonical_id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files, 0);
        assert!(owner.installed(&id).unwrap().is_empty());
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/user.zip")).unwrap(),
            b"unowned pack bytes"
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn selected_pack_nonprimary_member_preserves_the_reviewed_payload() {
        let (root, owner, id) = fixture().await;
        let plan = selected_pack_plan(&owner, &id, false).await.unwrap();
        assert!(plan.resolution().conflicts.is_empty());
        assert_eq!(plan.resolution().items.len(), 1);
        let selected = &plan.resolution().items[0];
        assert_eq!(selected.file.filename, "chosen.zip");
        assert_eq!(
            selected.file.url,
            "https://cdn.example.com/archive-member.zip"
        );
        assert_eq!(selected.file.size, Some(5));
        assert_eq!(
            selected.file.sha512.as_deref(),
            Some(Proof::bytes(b"known").sha512.as_str())
        );
        let instance = owner.directories.admit(&id).unwrap();
        let receipt = owner.prepare_install(&instance, &plan, false).unwrap();
        assert_eq!(receipt.changes.len(), 1);
        assert_eq!(receipt.changes[0].path, "resourcepacks/chosen.zip");
        validate_receipt(&receipt).unwrap();
        preflight(instance.game_directory(), &receipt, false).unwrap();
        owner.persist_receipt(&receipt).unwrap();
        owner
            .apply(
                instance,
                receipt,
                HashMap::from([("resourcepacks/chosen.zip".into(), b"known".to_vec())]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let directory = root.path().join("instances").join(id.as_str());
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/chosen.zip")).unwrap(),
            b"known"
        );
        assert!(!directory.join("resourcepacks/primary.zip").exists());
        assert!(!directory.join("resourcepacks/known.zip").exists());
        assert!(!directory.join("config/settings.txt").exists());
        let installed = owner.installed(&id).unwrap();
        assert_eq!(installed.entries().len(), 1);
        let member = installed
            .find(&CanonicalId("modrinth:known".into()))
            .unwrap();
        assert_eq!(member.version_id(), "known-v1");
        assert_eq!(member.managed_filename().unwrap().as_str(), "chosen.zip");
        assert_eq!(
            member.sha512(),
            Some(Proof::bytes(b"known").sha512.as_str())
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn selected_pack_same_version_replaces_the_disabled_artifact() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        let member_id = CanonicalId("modrinth:known".into());
        let mut before = ContentManifest::default();
        before
            .try_upsert(
                ManifestEntry::managed(
                    member_id.clone(),
                    ProviderId::Modrinth,
                    "known".into(),
                    "known-v1".into(),
                    ContentKind::ResourcePack,
                    &FileRef {
                        filename: "primary.zip".into(),
                        url: "https://cdn.example.com/primary.zip".into(),
                        size: Some(7),
                        sha512: Some(Proof::bytes(b"primary").sha512),
                        sha1: None,
                        primary: true,
                    },
                    Vec::new(),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        before.try_set_enabled(&member_id, false).unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            before.encode_managed().unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("resourcepacks/primary.zip.disabled"),
            b"primary",
        )
        .unwrap();
        std::fs::write(directory.join("resourcepacks/user.zip"), b"user bytes").unwrap();
        let plan = selected_pack_plan(&owner, &id, false).await.unwrap();
        let selected = &plan.resolution().items[0];
        assert!(selected.already_installed && selected.update);
        let instance = owner.directories.admit(&id).unwrap();
        let receipt = owner.prepare_install(&instance, &plan, false).unwrap();
        assert_eq!(receipt.changes.len(), 2);
        let after = ContentManifest::decode_managed(Some(&receipt.after_manifest)).unwrap();
        assert!(after.find(&member_id).unwrap().enabled());
        validate_receipt(&receipt).unwrap();
        preflight(instance.game_directory(), &receipt, false).unwrap();
        owner.persist_receipt(&receipt).unwrap();
        owner
            .apply(
                instance,
                receipt,
                HashMap::from([("resourcepacks/chosen.zip".into(), b"known".to_vec())]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/chosen.zip")).unwrap(),
            b"known"
        );
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/user.zip")).unwrap(),
            b"user bytes"
        );
        assert!(
            !directory
                .join("resourcepacks/primary.zip.disabled")
                .exists()
        );
        assert!(!directory.join("resourcepacks/chosen.zip.disabled").exists());
        assert!(!directory.join("config/settings.txt").exists());
        let installed = owner.installed(&id).unwrap();
        let member = installed.find(&member_id).unwrap();
        assert_eq!(member.version_id(), "known-v1");
        assert!(member.enabled());
        assert_eq!(
            member.sha512(),
            Some(Proof::bytes(b"known").sha512.as_str())
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn selected_pack_member_refuses_an_unselected_dependency() {
        let (_root, owner, id) = fixture().await;
        assert!(matches!(
            selected_pack_plan(&owner, &id, true).await,
            Err(MutationError::Pack(
                super::super::packs::PackError::SelectionChanged
            ))
        ));
        assert!(owner.installed(&id).unwrap().is_empty());
        assert!(!owner.has_unsettled_effects());
    }

    async fn selected_pack_plan(
        owner: &ContentMutations,
        id: &InstanceId,
        requires_dependency: bool,
    ) -> Result<TargetedPlan, MutationError> {
        let hash = Proof::bytes(b"known").sha512;
        let published_file = |name: &str, bytes: &[u8], primary: bool| {
            serde_json::json!({
                "filename":name, "url":format!("https://cdn.example.com/{name}"),
                "size":bytes.len(), "primary":primary,
                "hashes":{"sha512":Proof::bytes(bytes).sha512},
            })
        };
        let dependencies = if requires_dependency {
            serde_json::json!([{"project_id":"dependency", "dependency_type":"required"}])
        } else {
            serde_json::json!([])
        };
        let version = serde_json::json!({
            "project_id":"known", "id":"known-v1", "name":"Known version", "version_number":"1",
            "version_type":"release", "loaders":["minecraft"], "game_versions":["1.21.4"],
            "files":[published_file("primary.zip", b"primary", !requires_dependency),
                published_file("known.zip", b"known", requires_dependency)],
            "dependencies":dependencies,
        });
        let metadata = serde_json::json!([{
            "id":"known", "title":"Known pack member", "project_type":"resourcepack",
        }]);
        let mut responses = vec![
            serde_json::json!({hash.clone():version.clone()}).to_string(),
            metadata.to_string(),
            metadata.to_string(),
            serde_json::json!([version]).to_string(),
        ];
        if requires_dependency {
            responses.extend([
                serde_json::json!([{
                    "id":"dependency", "title":"Required dependency", "project_type":"resourcepack",
                }]).to_string(),
                serde_json::json!([{
                    "project_id":"dependency", "id":"dependency-v1", "name":"Dependency", "version_number":"1",
                    "version_type":"release", "loaders":["minecraft"], "game_versions":["1.21.4"],
                    "files":[published_file("dependency.zip", b"dependency", true)], "dependencies":[],
                }]).to_string(),
            ]);
        }
        let (service, provider) = metadata_provider(responses).await;
        let pack = pack_with_index(
            serde_json::json!({"name":"Pack fixture", "dependencies":{"minecraft":"1.21.4"},
                "files":[{"path":"resourcepacks/chosen.zip", "hashes":{"sha512":hash},
                    "downloads":["https://cdn.example.com/archive-member.zip"]}]}),
            &[("overrides/config/settings.txt", b"must not copy")],
        );
        let target = super::super::resolve::validated_target("vanilla", "1.21.4").unwrap();
        let preview = super::super::packs::preview_files(&service, pack, &target, &[])
            .await
            .unwrap();
        let shown = preview.snapshot();
        assert_eq!(shown.files.len(), 1);
        assert!(
            shown.files[0].identified && shown.files[0].compatible && !shown.files[0].installed
        );
        let selection = preview
            .select(&[shown.files[0].selection_id.clone()])
            .unwrap();
        let instance = owner.directories.admit(id).unwrap();
        let plan = owner
            .plan_selected_pack_admitted(&service, &instance, selection)
            .await;
        provider.await.unwrap();
        plan
    }

    #[tokio::test]
    async fn full_pack_installs_nested_and_empty_overrides_and_verifies_creation_retries() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        std::fs::write(directory.join("resourcepacks/user.zip"), b"user bytes").unwrap();
        let pack = pack(
            &[],
            &[
                ("overrides/config/nested/settings.txt", b"common"),
                ("client-overrides/config/nested/settings.txt", b"client"),
                ("overrides/options.txt", b"options"),
                ("overrides/config/empty.txt", b""),
            ],
        );
        let result = owner
            .install_pack(&catalog(&owner), &id, pack.clone(), true)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files, 3);
        assert_eq!(
            std::fs::read(directory.join("config/nested/settings.txt")).unwrap(),
            b"client"
        );
        assert_eq!(
            std::fs::read(directory.join("options.txt")).unwrap(),
            b"options"
        );
        assert_eq!(
            std::fs::read(directory.join("config/empty.txt")).unwrap(),
            b""
        );
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/user.zip")).unwrap(),
            b"user bytes"
        );
        assert!(!owner.has_unsettled_effects());
        assert_eq!(
            owner
                .install_pack(&catalog(&owner), &id, pack.clone(), true)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap()
                .changed_files,
            0
        );
        std::fs::write(directory.join("config/nested/settings.txt"), b"user edit").unwrap();
        assert!(matches!(
            owner
                .install_pack(&catalog(&owner), &id, pack.clone(), true)
                .unwrap()
                .join()
                .await
                .unwrap(),
            Err(MutationError::Changed)
        ));
        owner
            .remove(&id, &pack.canonical_id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(directory.join("config/nested/settings.txt")).unwrap(),
            b"user edit"
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn full_pack_honors_excluded_overrides() {
        let (root, owner, id) = fixture().await;
        let pack = pack(&[], &[("overrides/config/settings.txt", b"client")]);
        let result = owner
            .install_pack(&catalog(&owner), &id, pack, false)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.changed_files, 0);
        assert!(
            !root
                .path()
                .join("instances")
                .join(id.as_str())
                .join("config/settings.txt")
                .exists()
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn pack_collisions_preserve_user_files_and_leave_no_false_pending_fence() {
        for (existing, destination) in [
            ("config/settings.txt", "config/settings.txt"),
            ("resourcepacks/user.zip.disabled", "resourcepacks/user.zip"),
            ("Assets/user.txt", "assets/settings.txt"),
        ] {
            let (root, owner, id) = fixture().await;
            let directory = root.path().join("instances").join(id.as_str());
            let existing = directory.join(existing);
            std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
            std::fs::write(&existing, b"user bytes").unwrap();
            let archive_path = format!("overrides/{destination}");
            let pack = pack(&[], &[(archive_path.as_str(), b"pack bytes")]);
            assert!(
                owner
                    .install_pack(&catalog(&owner), &id, pack, true)
                    .unwrap()
                    .join()
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(std::fs::read(&existing).unwrap(), b"user bytes");
            assert!(!directory.join(MANIFEST_FILE).exists());
            assert!(!owner.has_unsettled_effects());
            assert!(owner.directories.admit(&id).is_ok());
        }
    }

    #[tokio::test]
    async fn pack_transaction_streams_nested_indexed_files_and_cancels_owned_stages() {
        for cancelled in [false, true] {
            let (root, owner, id) = fixture().await;
            let instance = owner.directories.admit(&id).unwrap();
            let pack = pack(
                &[("config/downloaded/data.bin", b"download")],
                &[("overrides/config/settings.txt", b"override")],
            );
            let receipt = pack_record(&instance, &pack);
            owner.persist_receipt(&receipt).unwrap();
            let plan = pack.archive.plan_all(true).unwrap();
            let cancel = CancellationToken::new();
            if cancelled {
                cancel.cancel();
            }
            let result = owner
                .apply_pack(
                    instance,
                    receipt,
                    HashMap::from([("config/downloaded/data.bin".into(), b"download".to_vec())]),
                    Some(&plan),
                    &cancel,
                )
                .await;
            let directory = root.path().join("instances").join(id.as_str());
            if cancelled {
                assert!(matches!(result, Err(MutationError::Cancelled)));
                assert!(!directory.join("config/downloaded").exists());
                assert!(!directory.join("config/settings.txt").exists());
                assert!(!directory.join(MANIFEST_FILE).exists());
            } else {
                assert_eq!(result.unwrap().changed_files, 2);
                assert_eq!(
                    std::fs::read(directory.join("config/downloaded/data.bin")).unwrap(),
                    b"download"
                );
                assert_eq!(
                    std::fs::read(directory.join("config/settings.txt")).unwrap(),
                    b"override"
                );
            }
            assert!(!owner.has_unsettled_effects());
            assert!(owner.directories.admit(&id).is_ok());
        }
    }

    #[tokio::test]
    async fn content_progress_reports_verified_payloads_and_commit_without_terminal_state() {
        let (_root, owner, id) = fixture().await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let reporting = owner.clone().with_progress(Arc::new(move |event| {
            received.lock().unwrap().push(event);
        }));
        let pack = pack(
            &[],
            &[
                ("overrides/config/empty.txt", b""),
                ("overrides/config/settings.txt", b"client"),
                ("overrides/options.txt", b"options"),
            ],
        );
        reporting
            .install_pack(&catalog(&owner), &id, pack.clone(), true)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let captured = events.lock().unwrap().clone();
        assert!(
            captured
                .iter()
                .all(|event| !event.done && event.file.is_none() && event.error.is_none())
        );
        let downloads = captured
            .iter()
            .filter(|event| event.phase == "content_download")
            .collect::<Vec<_>>();
        assert_eq!(
            downloads
                .iter()
                .map(|event| event.current)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert!(
            downloads
                .iter()
                .all(|event| event.total == 3 && event.bytes_total == Some(13))
        );
        assert_eq!(downloads.first().unwrap().bytes_done, Some(0));
        assert_eq!(downloads.last().unwrap().bytes_done, Some(13));
        assert!(
            downloads
                .windows(2)
                .all(|events| events[0].bytes_done <= events[1].bytes_done)
        );
        let commits = captured
            .iter()
            .filter(|event| event.phase == "content_commit")
            .collect::<Vec<_>>();
        assert_eq!(
            commits
                .iter()
                .map(|event| event.current)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(commits.iter().all(|event| event.total == 1));
        assert_eq!(captured.last().unwrap().phase, "content_commit");
        assert!(!owner.has_unsettled_effects());

        owner
            .remove(&id, &pack.canonical_id)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            captured,
            "the observer belongs only to its operation clone"
        );
    }

    #[tokio::test]
    async fn failed_or_cancelled_content_never_reports_commit_or_unverified_bytes() {
        for cancelled in [false, true] {
            let (root, owner, id) = fixture().await;
            let cancel = CancellationToken::new();
            let cancel_after_transfer = cancel.clone();
            let events = Arc::new(Mutex::new(Vec::new()));
            let received = events.clone();
            let owner = owner.with_progress(Arc::new(move |event| {
                if cancelled && event.phase == "content_download" && event.current == 1 {
                    cancel_after_transfer.cancel();
                }
                received.lock().unwrap().push(event);
            }));
            let instance = owner.directories.admit(&id).unwrap();
            let pack = pack(
                &[("config/downloaded/data.bin", b"download")],
                &[("overrides/config/settings.txt", b"override")],
            );
            let receipt = pack_record(&instance, &pack);
            owner.persist_receipt(&receipt).unwrap();
            let plan = pack.archive.plan_all(true).unwrap();
            let bytes = if cancelled {
                b"download".to_vec()
            } else {
                b"incorrect".to_vec()
            };
            let result = owner
                .apply_pack(
                    instance,
                    receipt,
                    HashMap::from([("config/downloaded/data.bin".into(), bytes)]),
                    Some(&plan),
                    &cancel,
                )
                .await;
            assert!(
                if cancelled {
                    matches!(result, Err(MutationError::Cancelled))
                } else {
                    matches!(result, Err(MutationError::Integrity))
                },
                "unexpected transfer outcome: {result:?}"
            );
            let captured = events.lock().unwrap();
            assert!(
                captured
                    .iter()
                    .all(|event| event.phase == "content_download" && !event.done)
            );
            let last = captured.last().unwrap();
            assert_eq!(last.current, i32::from(cancelled));
            assert_eq!(last.bytes_done, Some(if cancelled { 8 } else { 0 }));
            assert_eq!(last.bytes_total, Some(16));
            let directory = root.path().join("instances").join(id.as_str());
            assert!(!directory.join("config/downloaded/data.bin").exists());
            assert!(!directory.join("config/settings.txt").exists());
            assert!(!directory.join(MANIFEST_FILE).exists());
            assert!(!owner.has_unsettled_effects());
        }
    }

    #[tokio::test]
    async fn pack_restart_requires_native_settlement_and_verifies_all_published_destinations() {
        for native_settled in [false, true] {
            let (root, owner, id) = fixture().await;
            let instance = owner.directories.admit(&id).unwrap();
            let pack = pack(&[], &[("overrides/config/settings.txt", b"override")]);
            let mut receipt = pack_record(&instance, &pack);
            owner.persist_receipt(&receipt).unwrap();
            let directory = root.path().join("instances").join(id.as_str());
            std::fs::create_dir_all(directory.join("config")).unwrap();
            std::fs::write(directory.join("config/settings.txt"), b"override").unwrap();
            std::fs::write(directory.join(MANIFEST_FILE), &receipt.after_manifest).unwrap();
            if native_settled {
                let raw = encoded_receipt(&receipt).unwrap();
                owner.mark_native_settled(&mut receipt, &raw).unwrap();
            }
            drop(instance);
            let result = owner.resume(&id).unwrap().join().await.unwrap();
            if native_settled {
                assert_eq!(result.unwrap().changed_files, 1);
                let instance = owner.directories.admit(&id).unwrap();
                assert!(
                    owner
                        .installed_pack_admitted(
                            &instance,
                            &pack.canonical_id,
                            &pack.version_id,
                            pack.archive.fingerprint()
                        )
                        .unwrap()
                );
                assert!(!owner.has_unsettled_effects());
            } else {
                assert!(matches!(result, Err(MutationError::Pending)));
                assert!(matches!(
                    owner.resume(&id).unwrap().join().await.unwrap(),
                    Err(MutationError::Pending)
                ));
                assert!(owner.has_unsettled_effects());
                assert!(owner.directories.admit(&id).is_err());
            }
            assert_eq!(
                std::fs::read(directory.join("config/settings.txt")).unwrap(),
                b"override"
            );
        }
    }

    #[tokio::test]
    async fn hard_exit_after_content_staging_rolls_back_without_publication() {
        content_crash_restart(false, None).await;
    }

    #[tokio::test]
    async fn hard_exit_after_512_content_payloads_rolls_back_recorded_prefix() {
        content_crash_restart(false, Some("prefix")).await;
    }

    #[tokio::test]
    async fn hard_exit_after_content_parent_preparation_rolls_back_empty_parents() {
        content_crash_restart(false, Some("parents")).await;
    }

    #[tokio::test]
    async fn hard_exit_after_content_metadata_commit_preserves_publication() {
        content_crash_restart(true, None).await;
    }

    #[tokio::test]
    async fn hard_exit_ready_checkpoint_binding_mismatch_preserves_fence() {
        content_crash_restart(false, Some("binding")).await;
    }

    #[tokio::test]
    async fn hard_exit_ready_checkpoint_missing_preserves_fence() {
        content_crash_restart(false, Some("missing")).await;
    }

    #[tokio::test]
    async fn hard_exit_ready_checkpoint_foreign_public_file_allows_preserved_exit() {
        content_crash_restart(false, Some("foreign")).await;
    }

    #[tokio::test]
    async fn hard_exit_ready_rollback_delete_refusal_retries_after_reopen() {
        content_crash_restart(false, Some("delete")).await;
    }

    #[tokio::test]
    async fn hard_exit_after_content_replacement_restores_original_installation() {
        use crate::library::LibraryId;
        use axial_minecraft::download::{TransferClient, TransferClientConfig};
        use std::{io::Read, path::Path, process::Command, sync::mpsc, time::Duration};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const ROOT: &str = "AXIAL_CONTENT_REPLACEMENT_CRASH_ROOT";
        const LIBRARY: &str = "AXIAL_CONTENT_REPLACEMENT_CRASH_LIBRARY";
        const UNSAVED: &str = "AXIAL_CONTENT_REPLACEMENT_CRASH_UNSAVED";
        const ORIGINAL: &[u8] = b"original content payload";
        const REPLACEMENT: &[u8] = b"replacement content payload";
        const PAYLOAD: &str = "resourcepacks/replacement.zip";
        const WITNESS: &str = "original-tree.json";
        const UNSAVED_RECEIPT: &str = "unsaved-receipt.json";
        const UNSAVED_TREE: &str = "unsaved-tree.json";

        fn publication_checkpoint(raw: &str, saved: bool) -> serde_json::Value {
            assert!(raw.len() <= MAX_RECEIPT_BYTES);
            let receipt: Receipt = serde_json::from_str(raw).unwrap();
            validate_receipt(&receipt).unwrap();
            assert!(!receipt.native_settled);
            assert_eq!(receipt.changes.len(), 1);
            assert_eq!(receipt.changes[0].path, PAYLOAD);
            assert_eq!(receipt.changes[0].before, Some(Proof::bytes(ORIGINAL)));
            assert_eq!(receipt.changes[0].after, Some(Proof::bytes(REPLACEMENT)));
            let checkpoint = ManagedContentStagingCheckpoint::decode(
                receipt.ready_checkpoint.as_deref().unwrap(),
            )
            .unwrap();
            let checkpoint: serde_json::Value =
                serde_json::from_str(&checkpoint.encode(MAX_RECEIPT_BYTES).unwrap()).unwrap();
            if saved {
                let published = checkpoint["published"].as_array().unwrap();
                assert_eq!(published.len(), 1);
                assert_eq!(published[0]["path"], PAYLOAD);
                assert!(published[0]["backup"].is_object());
            } else {
                assert!(checkpoint.get("published").is_none());
            }
            assert!(checkpoint.get("restored").is_none());
            checkpoint
        }

        fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
            let mut pending = vec![root.to_path_buf()];
            let mut files = BTreeMap::new();
            let mut bytes = 0;
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(directory).unwrap() {
                    let path = entry.unwrap().path();
                    let relative = path.strip_prefix(root).unwrap().to_path_buf();
                    assert!(files.len() < 64 && relative.components().count() <= 4);
                    let metadata = std::fs::symlink_metadata(&path).unwrap();
                    let value = if metadata.is_dir() {
                        pending.push(path);
                        None
                    } else {
                        assert!(metadata.is_file() && metadata.len() <= 16 * 1024);
                        let mut body = Vec::new();
                        std::fs::File::open(&path)
                            .unwrap()
                            .take(16 * 1024 + 1)
                            .read_to_end(&mut body)
                            .unwrap();
                        assert_eq!(body.len() as u64, metadata.len());
                        bytes += body.len();
                        assert!(bytes <= 64 * 1024);
                        Some(body)
                    };
                    files.insert(relative, value);
                }
            }
            files
        }

        async fn plan(
            owner: &ContentMutations,
            id: &InstanceId,
            url: &reqwest::Url,
            version: &str,
            bytes: &[u8],
        ) -> TargetedPlan {
            let (service, provider) = metadata_provider(vec![
                serde_json::json!([{
                    "id":"replacement", "title":"Replacement fixture", "project_type":"resourcepack",
                }]).to_string(),
                serde_json::json!([{
                    "project_id":"replacement", "id":version, "name":version, "version_number":version,
                    "version_type":"release", "loaders":["minecraft"], "game_versions":["1.21.4"],
                    "files":[{"filename":"replacement.zip", "url":url.as_str(), "size":bytes.len(),
                        "primary":true, "hashes":{"sha512":Proof::bytes(bytes).sha512}}], "dependencies":[],
                }]).to_string(),
            ]).await;
            let plan = owner
                .plan(
                    &service,
                    id,
                    &[ResolutionSelection {
                        canonical_id: "modrinth:replacement".into(),
                        kind: ContentKind::ResourcePack,
                        version_id: Some(version.into()),
                    }],
                )
                .await
                .unwrap();
            provider.await.unwrap();
            plan
        }

        if let Some(root) = std::env::var_os(ROOT) {
            let unsaved = std::env::var(UNSAVED).unwrap() == "1";
            let root = std::path::PathBuf::from(root);
            assert_eq!(root.canonicalize().unwrap(), root);
            let library_id = LibraryId::parse(&std::env::var(LIBRARY).unwrap()).unwrap();
            let mut owner = open_crash_fixture(&root, library_id);
            let instances = InstanceService::new(owner.directories.clone(), owner.tasks.clone());
            let instance = instances
                .create(
                    CreateInstanceRequest {
                        name: "Content replacement crash fixture".into(),
                        selection_id: "vanilla|1.21.4".into(),
                        ..Default::default()
                    },
                    CreateTarget {
                        selection_id: "vanilla|1.21.4".into(),
                        version_id: "1.21.4".into(),
                        minecraft_version: "1.21.4".into(),
                        loader_key: "vanilla".into(),
                    },
                )
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            let game = root.join("instances").join(instance.id.as_str());
            std::fs::write(game.join("config/user.txt"), b"unrelated payload").unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = reqwest::Url::parse(&format!(
                "http://{}/replacement.zip",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let transport = tokio::spawn(async move {
                for bytes in [ORIGINAL, REPLACEMENT] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let mut chunk = [0; 1024];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0 && request.len() + count <= 8192);
                        request.extend_from_slice(&chunk[..count]);
                    }
                    assert!(request.starts_with(b"GET /replacement.zip HTTP/1.1\r\n"));
                    socket
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes())
                        .await
                        .unwrap();
                    socket.write_all(bytes).await.unwrap();
                }
            });
            let client = TransferClient::build(
                TransferClientConfig::bounded(
                    Duration::from_secs(5),
                    Duration::from_secs(5),
                    Duration::from_secs(10),
                    vec![TransferOrigin::from_loopback_http_for_test_support(&url).unwrap()],
                )
                .unwrap(),
            )
            .unwrap();
            owner.transfer_fixture = Some((url.clone(), client));
            let initial = plan(&owner, &instance.id, &url, "v1", ORIGINAL).await;
            let installed = owner
                .install(initial, false)
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(installed.changed_files, 1);
            assert_eq!(std::fs::read(game.join(PAYLOAD)).unwrap(), ORIGINAL);
            let original_manifest = owner
                .installed(&instance.id)
                .unwrap()
                .encode_managed()
                .unwrap();
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                original_manifest
            );
            let witness = serde_json::to_vec(&files(&game)).unwrap();
            assert!(witness.len() <= 128 * 1024);
            std::fs::write(root.join(WITNESS), witness).unwrap();
            let replacement = plan(&owner, &instance.id, &url, "v2", REPLACEMENT).await;
            let registry = owner.directories.registry().clone();
            let id = instance.id.clone();
            let observed_game = game.clone();
            let receipt_path = root.join(UNSAVED_RECEIPT);
            let verify_exit = move |raw: &str| -> ! {
                let checkpoint = publication_checkpoint(raw, !unsaved);
                let receipt: Receipt = serde_json::from_str(raw).unwrap();
                assert_eq!(
                    receipt.before_manifest.as_deref(),
                    Some(original_manifest.as_slice())
                );
                assert_eq!(
                    std::fs::read(observed_game.join(PAYLOAD)).unwrap(),
                    REPLACEMENT
                );
                assert_eq!(
                    std::fs::read(observed_game.join(MANIFEST_FILE)).unwrap(),
                    original_manifest
                );
                let private = observed_game.join(checkpoint["private_name"].as_str().unwrap());
                assert_eq!(std::fs::read_dir(private.join("stage")).unwrap().count(), 0);
                let backup = std::fs::read_dir(private.join("backup"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect::<Vec<_>>();
                assert_eq!(backup.len(), 1);
                assert_eq!(
                    std::fs::metadata(&backup[0]).unwrap().len(),
                    ORIGINAL.len() as u64
                );
                assert_eq!(std::fs::read(&backup[0]).unwrap(), ORIGINAL);
                if unsaved {
                    let witness = serde_json::to_vec(&files(&observed_game)).unwrap();
                    assert!(witness.len() <= 128 * 1024);
                    std::fs::write(root.join(UNSAVED_TREE), witness).unwrap();
                }
                std::process::exit(42);
            };
            let observer = if unsaved {
                let (begin, started) = mpsc::sync_channel(1);
                let (locked, admitted) = mpsc::sync_channel(1);
                let admitted = Mutex::new(admitted);
                let observer = std::thread::spawn(move || {
                    started.recv_timeout(Duration::from_secs(10)).unwrap();
                    registry
                        .storage()
                        .transaction(|db| -> Result<(), StorageError> {
                            let raw: String = db.query_row(
                                "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                                [id.as_str()],
                                |row| row.get(0),
                            )?;
                            let checkpoint = publication_checkpoint(&raw, false);
                            assert_eq!(std::fs::read(game.join(PAYLOAD)).unwrap(), ORIGINAL);
                            let private = game.join(checkpoint["private_name"].as_str().unwrap());
                            assert_eq!(
                                std::fs::read_dir(private.join("backup")).unwrap().count(),
                                0
                            );
                            let payloads = checkpoint["payloads"].as_array().unwrap();
                            assert_eq!(payloads.len(), 1);
                            assert_eq!(
                                std::fs::read(
                                    private
                                        .join("stage")
                                        .join(payloads[0]["name"].as_str().unwrap())
                                )
                                .unwrap(),
                                REPLACEMENT
                            );
                            std::fs::write(receipt_path, &raw).unwrap();
                            locked.send(()).unwrap();
                            let deadline = std::time::Instant::now() + Duration::from_secs(10);
                            while std::fs::read(game.join(PAYLOAD)).ok().as_deref()
                                != Some(REPLACEMENT)
                                && std::time::Instant::now() < deadline
                            {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            let unchanged: String = db.query_row(
                                "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                                [id.as_str()],
                                |row| row.get(0),
                            )?;
                            assert!(unchanged == raw, "publication proof must remain unsaved");
                            verify_exit(&raw)
                        })
                        .unwrap();
                });
                owner = owner.with_progress(Arc::new(move |event| {
                    if event.phase == "content_commit" && event.current == 0 {
                        begin.send(()).unwrap();
                        admitted
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                }));
                Some(observer)
            } else {
                ManagedContentTransactionRoot::before_manifest_revalidation_for_test(
                    &game.canonicalize().unwrap(),
                    move || {
                        let raw: String = registry
                            .storage()
                            .read(|db| {
                                db.query_row(
                                    "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                                    [id.as_str()],
                                    |row| row.get(0),
                                )
                                .map_err(StorageError::from)
                            })
                            .unwrap();
                        verify_exit(&raw);
                    },
                )
                .unwrap();
                None
            };
            let result = owner.install(replacement, false).unwrap().join().await;
            transport.await.unwrap();
            if let Some(observer) = observer {
                observer.join().unwrap();
            }
            panic!(
                "replacement did not reach its publication boundary (unsaved={unsaved}): {result:?}"
            );
        }

        for unsaved in [false, true] {
            let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let library_id = LibraryId::new();
            let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "content::install::tests::hard_exit_after_content_replacement_restores_original_installation", "--nocapture"])
            .env(ROOT, root.path())
            .env(LIBRARY, library_id.to_string())
            .env(UNSAVED, if unsaved { "1" } else { "0" })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let timed_out = child.try_wait().unwrap().is_none();
            if timed_out {
                child.kill().unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                !timed_out && output.status.code() == Some(42),
                "replacement crash boundary not reached: unsaved={unsaved}, timed_out={timed_out}, status={:?}; {} {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let mut witness = Vec::new();
            std::fs::File::open(
                root.path()
                    .join(if unsaved { UNSAVED_TREE } else { WITNESS }),
            )
            .unwrap()
            .take(128 * 1024 + 1)
            .read_to_end(&mut witness)
            .unwrap();
            assert!(witness.len() <= 128 * 1024);
            let expected_tree: BTreeMap<std::path::PathBuf, Option<Vec<u8>>> =
                serde_json::from_slice(&witness).unwrap();
            let mut restored_receipt = if unsaved {
                let mut raw = String::new();
                std::fs::File::open(root.path().join(UNSAVED_RECEIPT))
                    .unwrap()
                    .take(MAX_RECEIPT_BYTES as u64 + 1)
                    .read_to_string(&mut raw)
                    .unwrap();
                publication_checkpoint(&raw, false);
                Some(raw)
            } else {
                None
            };
            for attempt in 0..if unsaved { 2 } else { 3 } {
                let owner = open_crash_fixture(root.path(), library_id);
                let records = owner.directories.registry().list().unwrap();
                assert_eq!(records.len(), 1);
                let id = &records[0].instance.id;
                let game = root.path().join("instances").join(id.as_str());
                let storage = owner.directories.registry().storage();
                let pending = || {
                    storage
                        .read(|db| {
                            db.query_row(
                                "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                                [id.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(StorageError::from)
                        })
                        .unwrap()
                };
                let before = pending();
                if unsaved {
                    assert!(
                        before == restored_receipt,
                        "restart must retain the saved receipt"
                    );
                    assert_eq!(files(&game), expected_tree);
                    assert!(matches!(
                        owner.installed(id),
                        Err(MutationError::Unavailable)
                    ));
                    assert!(matches!(
                        owner.directories.admit_read(id),
                        Err(crate::instances::model::InstanceError::Busy)
                    ));
                    assert!(matches!(
                        owner.directories.admit(id),
                        Err(crate::instances::model::InstanceError::Busy)
                    ));
                } else if attempt == 0 {
                    assert!(before.is_some());
                    storage.transaction(|db| db.execute_batch(
                    "CREATE TRIGGER refuse_replacement_rollback_ack BEFORE DELETE ON content_batches BEGIN SELECT RAISE(IGNORE); END;"
                ).map_err(StorageError::from)).unwrap();
                } else {
                    assert_eq!(before, restored_receipt);
                }
                let result = if unsaved || attempt < 2 {
                    Some(owner.resume(id).unwrap().join().await.unwrap())
                } else {
                    None
                };
                let restored = files(&game);
                let installed = owner.installed(id);
                let after = pending();
                if unsaved || attempt == 0 {
                    assert!(owner.has_unsettled_effects());
                    assert!(matches!(
                        owner.directories.admit(id),
                        Err(crate::instances::model::InstanceError::Busy)
                    ));
                }
                if unsaved {
                    assert!(matches!(
                        owner.directories.admit_read(id),
                        Err(crate::instances::model::InstanceError::Busy)
                    ));
                }
                owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
                owner
                    .release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap())
                    .unwrap();
                owner.directories.library().try_preserve().unwrap();
                if let Some(result) = result {
                    assert!(
                        if unsaved || attempt == 0 {
                            matches!(result, Err(MutationError::Pending))
                        } else {
                            matches!(result, Err(MutationError::Cancelled))
                        },
                        "replacement recovery must respect its durable proof (unsaved={unsaved}, attempt={attempt}): {result:?}"
                    );
                }
                assert_eq!(restored, expected_tree);
                if unsaved || attempt == 0 {
                    assert!(matches!(installed, Err(MutationError::Unavailable)));
                } else {
                    assert_eq!(
                        installed
                            .unwrap()
                            .find(&CanonicalId("modrinth:replacement".into()))
                            .unwrap()
                            .version_id(),
                        "v1"
                    );
                }
                assert_eq!(pending(), after);
                if unsaved {
                    assert!(after == before, "Resume must preserve the unsaved receipt");
                    assert_eq!(files(&game), expected_tree);
                    assert!(owner.tasks.status().is_idle());
                    assert!(owner.has_unsettled_effects());
                    continue;
                }
                if attempt == 0 {
                    let before: Receipt = serde_json::from_str(before.as_deref().unwrap()).unwrap();
                    let receipt: Receipt = serde_json::from_str(after.as_deref().unwrap()).unwrap();
                    assert!(!receipt.native_settled);
                    assert_eq!(receipt.operation_id, before.operation_id);
                    assert_ne!(receipt.ready_checkpoint, before.ready_checkpoint);
                    let checkpoint = ManagedContentStagingCheckpoint::decode(
                        receipt.ready_checkpoint.as_deref().unwrap(),
                    )
                    .unwrap();
                    let checkpoint: serde_json::Value =
                        serde_json::from_str(&checkpoint.encode(MAX_RECEIPT_BYTES).unwrap())
                            .unwrap();
                    let before = ManagedContentStagingCheckpoint::decode(
                        before.ready_checkpoint.as_deref().unwrap(),
                    )
                    .unwrap();
                    let before: serde_json::Value =
                        serde_json::from_str(&before.encode(MAX_RECEIPT_BYTES).unwrap()).unwrap();
                    assert!(before.get("restored").is_none());
                    for field in ["files", "payloads", "published"] {
                        assert_eq!(checkpoint[field], before[field]);
                    }
                    let files = checkpoint["files"].as_array().unwrap();
                    let restored = checkpoint["restored"].as_array().unwrap();
                    assert_eq!(restored.len(), files.len());
                    let payload = files
                        .iter()
                        .position(|file| file["path"] == PAYLOAD)
                        .unwrap();
                    assert_eq!(restored[payload]["size"], ORIGINAL.len());
                    assert_eq!(restored[payload]["sha512"], Proof::bytes(ORIGINAL).sha512);
                    restored_receipt = after;
                    storage
                        .transaction(|db| {
                            db.execute_batch("DROP TRIGGER refuse_replacement_rollback_ack")
                                .map_err(StorageError::from)
                        })
                        .unwrap();
                    assert_eq!(pending(), restored_receipt);
                } else {
                    assert!(after.is_none());
                    assert!(!owner.has_unsettled_effects());
                    restored_receipt = None;
                }
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_update_requires_affected_row_and_exact_readback_before_commit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for rewrite in ["ignore", "old", "oversized"] {
            let (root, owner, id) = fixture().await;
            let game = root.path().join("instances").join(id.as_str());
            let original = std::fs::read(game.join(MANIFEST_FILE)).ok();
            let storage = owner.directories.registry().storage();
            storage.transaction(|db| db.execute_batch(match rewrite {
                "old" =>
                "CREATE TRIGGER refuse_checkpoint AFTER UPDATE ON content_batches
                 WHEN json_extract(NEW.receipt_json,'$.ready_checkpoint') IS NOT NULL
                 BEGIN UPDATE content_batches SET receipt_json=OLD.receipt_json WHERE instance_id=NEW.instance_id; END;",
                "oversized" =>
                "CREATE TRIGGER refuse_checkpoint AFTER UPDATE ON content_batches
                 WHEN json_extract(NEW.receipt_json,'$.ready_checkpoint') IS NOT NULL
                 BEGIN UPDATE content_batches SET receipt_json=replace(hex(zeroblob(8388609)),'00','é') WHERE instance_id=NEW.instance_id; END;",
                _ =>
                "CREATE TRIGGER refuse_checkpoint BEFORE UPDATE ON content_batches
                 WHEN json_extract(NEW.receipt_json,'$.ready_checkpoint') IS NOT NULL
                 BEGIN SELECT RAISE(IGNORE); END;",
            }).map_err(StorageError::from)).unwrap();
            let committing = Arc::new(AtomicBool::new(false));
            let observed = committing.clone();
            let reporting = owner.clone().with_progress(Arc::new(move |event| {
                if event.phase == "content_commit" {
                    observed.store(true, Ordering::SeqCst);
                }
            }));
            let result = reporting
                .install_pack(
                    &catalog(&owner),
                    &id,
                    pack(
                        &[],
                        &[("overrides/config/checkpoint.txt", b"not published")],
                    ),
                    true,
                )
                .unwrap()
                .join()
                .await
                .unwrap();
            owner
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
            owner.directories.library().try_preserve().unwrap();
            assert!(matches!(result, Err(MutationError::Changed)), "{result:?}");
            assert!(!committing.load(Ordering::SeqCst));
            assert!(!has_pending(storage, &id).unwrap());
            assert!(owner.pending.lock().unwrap().is_empty());
            assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).ok(), original);
            assert!(!game.join("config/checkpoint.txt").exists());
        }
    }

    #[tokio::test]
    async fn checkpoint_completion_cancellation_unwinds_before_publication() {
        let (root, owner, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        let original = std::fs::read(game.join(MANIFEST_FILE)).ok();
        let cancel = CancellationToken::new();
        let captured_cancel = cancel.clone();
        let registry = owner.directories.registry().clone();
        let captured_id = id.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured_events = events.clone();
        let owner = owner.with_progress(Arc::new(move |event| {
            if event.phase == "content_commit" {
                assert_eq!(event.current, 0);
                let recorded: String = registry
                    .storage()
                    .read(|db| {
                        db.query_row(
                            "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                            [captured_id.as_str()],
                            |row| row.get(0),
                        )
                        .map_err(StorageError::from)
                    })
                    .unwrap();
                let receipt: Receipt = serde_json::from_str(&recorded).unwrap();
                assert!(receipt.ready_checkpoint.is_some() && !receipt.native_settled);
                captured_events.lock().unwrap().push(event.current);
                captured_cancel.cancel();
            }
        }));
        let instance = owner.directories.admit(&id).unwrap();
        let pack = pack(&[], &[("overrides/config/cancelled.txt", b"not published")]);
        let record = pack_record(&instance, &pack);
        owner.persist_receipt(&record).unwrap();
        let plan = pack.archive.plan_all(true).unwrap();
        let result = owner
            .apply_pack(instance, record, HashMap::new(), Some(&plan), &cancel)
            .await;
        owner
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        owner.directories.library().try_preserve().unwrap();
        assert!(
            matches!(result, Err(MutationError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(*events.lock().unwrap(), [0]);
        assert!(!owner.has_unsettled_effects());
        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).ok(), original);
        assert!(!game.join("config/cancelled.txt").exists());
    }

    #[tokio::test]
    async fn live_content_recovery_completes_original_task_after_cleanup_refusal() {
        live_content_cleanup_refusal(false, false).await;
    }

    #[tokio::test]
    async fn live_content_recovery_keeps_cancelled_task_until_rollback_cleanup() {
        live_content_cleanup_refusal(true, false).await;
    }

    #[tokio::test]
    async fn live_content_recovery_settles_cleanup_when_receipt_cannot_fit_a_checkpoint() {
        use std::{process::Command, time::Duration};
        const CHILD: &str = "AXIAL_CONTENT_CAPACITY_CLEANUP_CHILD";
        if std::env::var_os(CHILD).is_some() {
            live_content_cleanup_refusal(true, true).await;
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "content::install::tests::live_content_recovery_settles_cleanup_when_receipt_cannot_fit_a_checkpoint", "--nocapture"])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out && output.status.success(),
            "ordinary cleanup must settle without an optional checkpoint: timed_out={timed_out}, status={:?}; {} {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn live_content_cleanup_refusal(cancel_before_commit: bool, near_capacity: bool) {
        use std::{
            io::Read,
            path::{Path, PathBuf},
            time::Duration,
        };

        const CANARY: &[u8] = b"not owned by the transaction";
        fn locate_canary(root: &Path, name: &str) -> io::Result<PathBuf> {
            if !std::fs::symlink_metadata(root)?.is_dir() {
                return Err(io::Error::other("fixture private root changed"));
            }
            let mut directories = vec![(root.to_path_buf(), 0)];
            let mut entries = 0;
            let mut found = None;
            while let Some((directory, depth)) = directories.pop() {
                for entry in std::fs::read_dir(directory)? {
                    let entry = entry?;
                    entries += 1;
                    let metadata = std::fs::symlink_metadata(entry.path())?;
                    if entries > 16 || metadata.file_type().is_symlink() {
                        return Err(io::Error::other("unexpected fixture tree"));
                    }
                    if metadata.is_dir() {
                        if depth >= 2 {
                            return Err(io::Error::other("fixture tree is too deep"));
                        }
                        directories.push((entry.path(), depth + 1));
                    } else if entry.file_name() == name {
                        let mut bytes = Vec::new();
                        std::fs::File::open(entry.path())?
                            .take(CANARY.len() as u64 + 1)
                            .read_to_end(&mut bytes)?;
                        if !metadata.is_file() || bytes != CANARY || found.is_some() {
                            return Err(io::Error::other("fixture canary is not exact and unique"));
                        }
                        found = Some(entry.path());
                    }
                }
            }
            found.ok_or_else(|| io::Error::other("fixture canary was not preserved"))
        }

        let (root, owner, id) = fixture().await;
        let game = root.path().join("instances").join(id.as_str());
        let removed = CanonicalId("modrinth:capacity-provenance".into());
        if near_capacity {
            let dependencies = (0..256)
                .map(|_| ContentDependency {
                    project_id: Some("p".repeat(512)),
                    version_id: Some("v".repeat(512)),
                    kind: super::super::model::DependencyKind::Optional,
                })
                .collect::<Vec<_>>();
            let mut manifest = ContentManifest::default();
            for index in 0..3 {
                let project = format!("capacity-{index}");
                let filename = format!("capacity-{index}.zip");
                let bytes = b"retained capacity fixture payload";
                manifest
                    .try_upsert(
                        ManifestEntry::managed(
                            CanonicalId::for_project(ProviderId::Modrinth, &project),
                            ProviderId::Modrinth,
                            project,
                            "v1".into(),
                            ContentKind::ResourcePack,
                            &FileRef {
                                filename: filename.clone(),
                                url: "https://example.invalid/capacity.zip".into(),
                                size: Some(bytes.len() as u64),
                                sha512: Some(Proof::bytes(bytes).sha512),
                                sha1: None,
                                primary: true,
                            },
                            dependencies.clone(),
                            None,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                std::fs::write(game.join("resourcepacks").join(filename), bytes).unwrap();
            }
            manifest
                .try_upsert(
                    ManifestEntry::provenance(
                        removed.clone(),
                        ProviderId::Modrinth,
                        removed.project_id().into(),
                        "v1".into(),
                        None,
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut original = manifest.encode_managed().unwrap();
            std::fs::write(game.join(MANIFEST_FILE), &original).unwrap();
            assert_eq!(owner.installed(&id).unwrap(), manifest);
            let measured = Arc::new(Mutex::new(None));
            let measured_by_task = measured.clone();
            let accepted = Arc::new(Mutex::new(None));
            let accepted_by_task = accepted.clone();
            let registry = owner.directories.registry().clone();
            let observed_id = id.clone();
            let tasks = owner.tasks.clone();
            let calibration = owner.clone().with_progress(Arc::new(move |event| {
                if event.phase == "content_commit" && event.current == 0 {
                    let raw: String = registry
                        .storage()
                        .read(|db| {
                            db.query_row(
                                "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                                [observed_id.as_str()],
                                |row| row.get(0),
                            )
                            .map_err(StorageError::from)
                        })
                        .unwrap();
                    let receipt: Receipt = serde_json::from_str(&raw).unwrap();
                    let checkpoint_bytes = receipt.ready_checkpoint.as_ref().map_or(0, |proof| {
                        ",\"ready_checkpoint\":".len() + serde_json::to_vec(proof).unwrap().len()
                    });
                    *measured_by_task.lock().unwrap() = Some(raw.len() - checkpoint_bytes);
                    assert!(
                        tasks.cancel(accepted_by_task.lock().unwrap().expect("calibration task"))
                    );
                }
            }));
            let task = calibration.remove(&id, &removed).unwrap();
            *accepted.lock().unwrap() = Some(task.id());
            let result = tokio::time::timeout(Duration::from_secs(10), task.join())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(result, Err(MutationError::Cancelled)),
                "{result:?}"
            );
            assert!(!owner.has_unsettled_effects());
            assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), original);
            let base = measured.lock().unwrap().unwrap();
            let padding = MAX_RECEIPT_BYTES.checked_sub(base + 8).unwrap() / 3;
            assert!(padding > 0 && original.len() + padding <= MAX_MANIFEST_BYTES);
            original.resize(original.len() + padding, b' ');
            std::fs::write(game.join(MANIFEST_FILE), original).unwrap();
        }
        let original_children = std::fs::read_dir(&game)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let registry = owner.directories.registry().clone();
        let captured_id = id.clone();
        let captured_game = game.clone();
        let accepted_task = Arc::new(Mutex::new(None));
        let captured_task = accepted_task.clone();
        let tasks = owner.tasks.clone();
        let canary_name = format!("foreign-cleanup-canary-{}", uuid::Uuid::new_v4());
        let captured_name = canary_name.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Mutex::new(Some(sender));
        let inject = move || {
            let raw: String = registry
                .storage()
                .read(|db| {
                    db.query_row(
                        "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                        [captured_id.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(StorageError::from)
                })
                .unwrap();
            let receipt: Receipt = serde_json::from_str(&raw).unwrap();
            let (private, publication_recorded) = if near_capacity {
                assert!(receipt.ready_checkpoint.is_none());
                assert!(
                    raw.len() <= MAX_RECEIPT_BYTES
                        && raw.len() + ",\"ready_checkpoint\":\"\"".len() > MAX_RECEIPT_BYTES
                );
                let children = std::fs::read_dir(&captured_game)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<std::collections::BTreeSet<_>>();
                assert!(children.len() <= 16 && original_children.is_subset(&children));
                let added = children.difference(&original_children).collect::<Vec<_>>();
                assert_eq!(added.len(), 1);
                let private = captured_game.join(added[0]);
                assert!(std::fs::symlink_metadata(&private).unwrap().is_dir());
                assert_eq!(std::fs::read_dir(&private).unwrap().count(), 2);
                assert_eq!(std::fs::read_dir(private.join("stage")).unwrap().count(), 0);
                assert_eq!(
                    std::fs::read_dir(private.join("backup")).unwrap().count(),
                    0
                );
                (private, true)
            } else {
                let checkpoint = ManagedContentStagingCheckpoint::decode(
                    receipt.ready_checkpoint.as_deref().unwrap(),
                )
                .unwrap();
                let checkpoint: serde_json::Value =
                    serde_json::from_str(&checkpoint.encode(MAX_RECEIPT_BYTES).unwrap()).unwrap();
                let publication_recorded = cancel_before_commit
                    || (checkpoint["published"]
                        .as_array()
                        .is_some_and(|published| published.len() == 1)
                        && std::fs::read(captured_game.join("config/live-recovery.txt"))
                            .ok()
                            .as_deref()
                            == Some(b"published")
                        && std::fs::read(captured_game.join(MANIFEST_FILE)).ok()
                            == receipt.before_manifest);
                (
                    captured_game.join(checkpoint["private_name"].as_str().unwrap()),
                    publication_recorded,
                )
            };
            let canary = private.join("stage").join(&captured_name);
            std::fs::write(&canary, CANARY).unwrap();
            let handle = std::fs::File::open(&canary).unwrap();
            if cancel_before_commit {
                assert!(tasks.cancel(captured_task.lock().unwrap().expect("accepted task")));
            }
            sender
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send((private, handle, receipt, publication_recorded))
                .unwrap();
        };
        let reporting = if cancel_before_commit {
            owner.clone().with_progress(Arc::new(move |event| {
                if event.phase == "content_commit" && event.current == 0 {
                    inject();
                }
            }))
        } else {
            ManagedContentTransactionRoot::before_manifest_revalidation_for_test(
                &game.canonicalize().unwrap(),
                inject,
            )
            .unwrap();
            owner.clone()
        };
        let task = if near_capacity {
            reporting.remove(&id, &removed)
        } else {
            reporting.install_pack(
                &catalog(&owner),
                &id,
                pack(&[], &[("overrides/config/live-recovery.txt", b"published")]),
                true,
            )
        }
        .unwrap();
        let task_id = task.id();
        *accepted_task.lock().unwrap() = Some(task_id);
        let mut joined = Box::pin(task.join());
        let (private, handle, receipt, publication_recorded) =
            tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .unwrap()
                .unwrap();
        let early = tokio::time::timeout(Duration::from_millis(200), &mut joined)
            .await
            .ok();
        let published = std::fs::read(game.join("config/live-recovery.txt")).ok();
        let manifest = std::fs::read(game.join(MANIFEST_FILE)).ok();
        let mut canary_bytes = Vec::new();
        let canary_read = handle
            .take(CANARY.len() as u64 + 1)
            .read_to_end(&mut canary_bytes);
        let canary = locate_canary(&private, &canary_name);
        let fenced = owner.directories.admit(&id).is_err();
        let task_retained = owner.tasks.status().running.contains(&task_id);
        let premature_recovery = owner.pending.lock().unwrap().get(&id).is_some_and(|batch| {
            batch
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::Recovery(_)))
        });
        let raw: Result<String, StorageError> = owner.directories.registry().storage().read(|db| {
            db.query_row(
                "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .map_err(StorageError::from)
        });

        // Native empty-directory cleanup can park stage; only remove our own file.
        let cleanup = canary.and_then(|path| {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.len() != CANARY.len() as u64 {
                return Err(io::Error::other("fixture canary changed before cleanup"));
            }
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take(CANARY.len() as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes != CANARY {
                return Err(io::Error::other(
                    "fixture canary bytes changed before cleanup",
                ));
            }
            std::fs::remove_file(path)
        });
        let result = if early.is_some() {
            // Settle the unfixed implementation before asserting its premature return.
            tokio::time::timeout(Duration::from_secs(5), owner.resume(&id).unwrap().join())
                .await
                .unwrap()
                .unwrap()
        } else {
            match tokio::time::timeout(Duration::from_secs(5), &mut joined).await {
                Ok(result) => result.unwrap(),
                Err(_) if near_capacity => {
                    assert!(cleanup.is_ok());
                    assert!(locate_canary(&private, &canary_name).is_err());
                    assert!(owner.tasks.status().running.contains(&task_id));
                    assert!(owner.directories.admit(&id).is_err());
                    eprintln!(
                        "accepted cancellation remains running after exact cleanup canary removal; fixture={}",
                        root.path().display()
                    );
                    std::process::exit(43);
                }
                Err(error) => panic!("live cleanup did not settle: {error}"),
            }
        };
        owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
        owner.directories.library().try_preserve().unwrap();

        cleanup.unwrap();
        canary_read.unwrap();
        assert!(
            publication_recorded,
            "cleanup fault must follow the saved post-move proof"
        );
        let expected_payload = (!cancel_before_commit).then(|| b"published".to_vec());
        let expected_manifest = if cancel_before_commit {
            receipt.before_manifest.clone()
        } else {
            Some(receipt.after_manifest.clone())
        };
        assert_eq!(published, expected_payload);
        assert_eq!(manifest, expected_manifest);
        assert_eq!(canary_bytes, CANARY);
        let blocked_receipt: Receipt = serde_json::from_str(&raw.unwrap()).unwrap();
        assert!(fenced && !blocked_receipt.native_settled);
        assert!(
            early.is_none(),
            "accepted task returned before its live native recovery settled: {early:?}; retained native recovery: {premature_recovery}"
        );
        assert!(
            task_retained,
            "original task must retain admission during recovery"
        );
        if cancel_before_commit {
            assert!(
                matches!(result, Err(MutationError::Cancelled)),
                "{result:?}"
            );
        } else {
            let result = result.unwrap();
            assert_eq!(result.operation_id, receipt.operation_id);
            assert_eq!(result.status, "complete");
        }
        assert!(!private.exists());
        assert!(!owner.has_unsettled_effects());
        assert_eq!(
            std::fs::read(game.join("config/live-recovery.txt")).ok(),
            expected_payload
        );
        assert_eq!(
            std::fs::read(game.join(MANIFEST_FILE)).ok(),
            expected_manifest
        );
        if near_capacity {
            for index in 0..3 {
                assert_eq!(
                    std::fs::read(game.join(format!("resourcepacks/capacity-{index}.zip")))
                        .unwrap(),
                    b"retained capacity fixture payload"
                );
            }
        }
    }

    #[tokio::test]
    async fn staged_prefix_checkpoint_refusal_cancels_before_reporting_completion() {
        for rewrite in [false, true] {
            let (root, owner, id) = fixture().await;
            let game = root.path().join("instances").join(id.as_str());
            let original = std::fs::read(game.join(MANIFEST_FILE)).ok();
            let original_children = std::fs::read_dir(&game)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<HashSet<_>>();
            let registry = owner.directories.registry().clone();
            let captured_id = id.clone();
            let events = Arc::new(Mutex::new(Vec::new()));
            let received = events.clone();
            let reporting = owner.clone().with_progress(Arc::new(move |event| {
                assert_ne!(event.phase, "content_commit");
                if event.phase != "content_download" { return; }
                received.lock().unwrap().push(event.current);
                if event.current != 511 { return; }
                let recorded: String = registry.storage().read(|db| db.query_row(
                    "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                    [captured_id.as_str()], |row| row.get(0),
                ).map_err(StorageError::from)).unwrap();
                let receipt: Receipt = serde_json::from_str(&recorded).unwrap();
                assert!(!receipt.native_settled && receipt.ready_checkpoint.is_some());
                registry.storage().transaction(|db| db.execute_batch(if rewrite {
                    "CREATE TRIGGER refuse_prefix AFTER UPDATE ON content_batches
                     BEGIN UPDATE content_batches SET receipt_json=OLD.receipt_json WHERE instance_id=NEW.instance_id; END;"
                } else {
                    "CREATE TRIGGER refuse_prefix BEFORE UPDATE ON content_batches BEGIN SELECT RAISE(IGNORE); END;"
                }).map_err(StorageError::from)).unwrap();
            }));
            let result = reporting
                .install_pack(&catalog(&owner), &id, prefix_pack(), true)
                .unwrap()
                .join()
                .await
                .unwrap();
            owner
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
            owner.directories.library().try_preserve().unwrap();
            assert!(matches!(result, Err(MutationError::Changed)), "{result:?}");
            assert_eq!(events.lock().unwrap().last(), Some(&511));
            assert!(!owner.has_unsettled_effects());
            assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).ok(), original);
            assert_eq!(
                std::fs::read_dir(&game)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<HashSet<_>>(),
                original_children
            );
            for index in 0..513 {
                assert!(!game.join(format!("config/prefix-{index:04}.txt")).exists());
            }
        }
    }

    #[tokio::test]
    async fn parent_checkpoint_refusal_removes_observed_parents_before_publication() {
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };
        for rewrite in [false, true] {
            let (root, owner, id) = fixture().await;
            let game = root.path().join("instances").join(id.as_str());
            assert!(!game.join("new-root").exists());
            let original = std::fs::read(game.join(MANIFEST_FILE)).ok();
            let before = original.clone();
            let observing_game = game.clone();
            let registry = owner.directories.registry().clone();
            let (begin, started) = mpsc::sync_channel(1);
            let (locked, admitted) = mpsc::sync_channel(1);
            let admitted = Mutex::new(admitted);
            let observer = std::thread::spawn(move || {
                if started.recv_timeout(Duration::from_secs(10)).is_err() {
                    return false;
                }
                registry.storage().transaction(|db| -> Result<bool, StorageError> {
                    db.execute_batch(if rewrite {
                        "CREATE TRIGGER refuse_parent_checkpoint AFTER UPDATE ON content_batches
                         WHEN EXISTS(SELECT 1 FROM json_each(json_extract(NEW.receipt_json,'$.ready_checkpoint'),'$.created_parents'))
                         BEGIN UPDATE content_batches SET receipt_json=OLD.receipt_json WHERE instance_id=NEW.instance_id; END;"
                    } else {
                        "CREATE TRIGGER refuse_parent_checkpoint BEFORE UPDATE ON content_batches
                         WHEN EXISTS(SELECT 1 FROM json_each(json_extract(NEW.receipt_json,'$.ready_checkpoint'),'$.created_parents'))
                         BEGIN SELECT RAISE(IGNORE); END;"
                    })?;
                    locked.send(()).unwrap();
                    let nested = observing_game.join("new-root/nested");
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !nested.is_dir() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(nested.is_dir()
                        && std::fs::read_dir(observing_game.join("new-root")).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>() == [std::ffi::OsString::from("nested")]
                        && std::fs::read_dir(nested).unwrap().count() == 0
                        && !observing_game.join("new-root/nested/staged.txt").exists()
                        && std::fs::read(observing_game.join(MANIFEST_FILE)).ok() == before)
                }).unwrap()
            });
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let reporting = owner.clone().with_progress(Arc::new(move |event| {
                captured
                    .lock()
                    .unwrap()
                    .push((event.phase.clone(), event.current));
                if event.phase == "content_download" && event.current == 1 {
                    begin.send(()).unwrap();
                    admitted
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            }));
            let result = reporting
                .install_pack(
                    &catalog(&owner),
                    &id,
                    pack(
                        &[],
                        &[("overrides/new-root/nested/staged.txt", b"new payload")],
                    ),
                    true,
                )
                .unwrap()
                .join()
                .await
                .unwrap();
            let observed = observer.join().unwrap();
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner.directories.library().try_preserve().unwrap();
            assert!(
                observed,
                "parent checkpoint refusal must follow actual empty-parent materialization"
            );
            assert!(matches!(result, Err(MutationError::Changed)), "{result:?}");
            assert!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(phase, _)| phase != "content_commit")
            );
            assert!(!owner.has_unsettled_effects());
            assert!(!game.join("new-root").exists());
            assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).ok(), original);
        }
    }

    #[tokio::test]
    async fn published_checkpoint_refusal_compensates_before_manifest_publication() {
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };

        for refusal in ["ignore", "old", "abort"] {
            let (root, owner, id) = fixture().await;
            owner
                .install_pack(
                    &catalog(&owner),
                    &id,
                    pack(&[], &[("overrides/config/preserved.txt", b"original")]),
                    true,
                )
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            let game = root.path().join("instances").join(id.as_str());
            let original_manifest = std::fs::read(game.join(MANIFEST_FILE)).unwrap();
            assert!(!game.join("config/published.txt").exists());
            let before_manifest = original_manifest.clone();
            let observing_game = game.clone();
            let registry = owner.directories.registry().clone();
            let (begin, started) = mpsc::sync_channel(1);
            let (locked, admitted) = mpsc::sync_channel(1);
            let admitted = Mutex::new(admitted);
            let observer = std::thread::spawn(move || {
                if started.recv_timeout(Duration::from_secs(10)).is_err() {
                    return false;
                }
                registry.storage().transaction(|db| -> Result<bool, StorageError> {
                    db.execute_batch(match refusal {
                        "old" =>
                        "CREATE TRIGGER refuse_published_checkpoint AFTER UPDATE ON content_batches
                         WHEN json_array_length(json_extract(NEW.receipt_json,'$.ready_checkpoint'),'$.published') > 0
                         BEGIN UPDATE content_batches SET receipt_json=OLD.receipt_json WHERE instance_id=NEW.instance_id; END;",
                        "abort" =>
                        "CREATE TRIGGER refuse_published_checkpoint BEFORE UPDATE ON content_batches
                         WHEN json_array_length(json_extract(NEW.receipt_json,'$.ready_checkpoint'),'$.published') > 0
                         BEGIN SELECT RAISE(ABORT, 'injected checkpoint refusal'); END;",
                        _ =>
                        "CREATE TRIGGER refuse_published_checkpoint BEFORE UPDATE ON content_batches
                         WHEN json_array_length(json_extract(NEW.receipt_json,'$.ready_checkpoint'),'$.published') > 0
                         BEGIN SELECT RAISE(IGNORE); END;",
                    })?;
                    locked.send(()).unwrap();
                    let published = observing_game.join("config/published.txt");
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !published.is_file() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(std::fs::read(&published).ok().as_deref() == Some(b"new payload")
                        && std::fs::read(observing_game.join("config/preserved.txt")).ok().as_deref() == Some(b"original")
                        && std::fs::read(observing_game.join(MANIFEST_FILE)).ok().as_deref() == Some(before_manifest.as_slice()))
                }).unwrap()
            });
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let reporting = owner.clone().with_progress(Arc::new(move |event| {
                if event.phase == "content_commit" {
                    captured.lock().unwrap().push(event.current);
                    if event.current == 0 {
                        begin.send(()).unwrap();
                        admitted
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                }
            }));
            let result = reporting
                .install_pack(
                    &catalog(&owner),
                    &id,
                    pack(&[], &[("overrides/config/published.txt", b"new payload")]),
                    true,
                )
                .unwrap()
                .join()
                .await
                .unwrap();
            let observed = observer.join().unwrap();
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner.directories.library().try_preserve().unwrap();

            assert!(
                observed,
                "refusal must follow public moves with the old manifest"
            );
            if refusal == "abort" {
                assert!(
                    matches!(result, Err(MutationError::Storage(_))),
                    "{result:?}"
                );
            } else {
                assert!(matches!(result, Err(MutationError::Changed)), "{result:?}");
            }
            assert_eq!(*events.lock().unwrap(), [0]);
            assert!(!owner.has_unsettled_effects());
            assert!(!game.join("config/published.txt").exists());
            assert_eq!(
                std::fs::read(game.join("config/preserved.txt")).unwrap(),
                b"original"
            );
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                original_manifest
            );
        }
    }

    #[tokio::test]
    async fn oversized_utf8_receipt_remains_fenced_without_decoding() {
        let (_root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let record = receipt(&owner, &instance, b"not published");
        drop(instance);
        let storage = owner.directories.registry().storage();
        storage.transaction(|db| db.execute(
            "UPDATE content_batches SET receipt_json=replace(hex(zeroblob(?1)),'00','é') WHERE instance_id=?2",
            params![MAX_RECEIPT_BYTES / 2 + 1, id.as_str()],
        ).map_err(StorageError::from)).unwrap();
        let result = owner.resume(&id).unwrap().join().await.unwrap();
        owner
            .tasks
            .shutdown(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        owner
            .release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap())
            .unwrap();
        owner.directories.library().try_preserve().unwrap();
        assert!(matches!(result, Err(MutationError::Pending)), "{result:?}");
        let persisted: (String, usize, usize) = storage.read(|db| db.query_row(
            "SELECT operation_id,length(receipt_json),length(CAST(receipt_json AS BLOB)) FROM content_batches WHERE instance_id=?1",
            [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).map_err(StorageError::from)).unwrap();
        assert_eq!(
            persisted,
            (
                record.operation_id,
                MAX_RECEIPT_BYTES / 2 + 1,
                MAX_RECEIPT_BYTES + 2
            )
        );
        assert!(owner.has_unsettled_effects());
        assert!(owner.directories.admit(&id).is_err());
    }

    #[tokio::test]
    async fn preserve_shutdown_refuses_retained_native_or_unacknowledged_batches() {
        for acknowledgement in ["UPDATE", "DELETE"] {
            let (root, owner, id) = fixture().await;
            let storage = owner.directories.registry().storage();
            let predicate = if acknowledgement == "UPDATE" {
                "WHEN json_extract(NEW.receipt_json, '$.native_settled') = 1"
            } else {
                ""
            };
            storage
                .transaction(|db| {
                    db.execute_batch(&format!(
                        "CREATE TRIGGER refuse_content_acknowledgement BEFORE {acknowledgement} ON content_batches {predicate} BEGIN SELECT RAISE(ABORT, 'injected content acknowledgement refusal'); END;"
                    ))
                    .map_err(StorageError::from)
                })
                .unwrap();
            assert!(matches!(
                owner
                    .install_pack(
                        &catalog(&owner),
                        &id,
                        pack(&[], &[("overrides/config/preserved.txt", b"published")]),
                        true,
                    )
                    .unwrap()
                    .join()
                    .await
                    .unwrap(),
                Err(MutationError::Pending)
            ));
            let recorded = || {
                storage
                    .read(|db| {
                        db.query_row(
                            "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                            [id.as_str()],
                            |row| row.get::<_, String>(0),
                        )
                        .map_err(StorageError::from)
                    })
                    .unwrap()
            };
            let before = recorded();
            let receipt: Receipt = serde_json::from_str(&before).unwrap();
            validate_receipt(&receipt).unwrap();
            let game = root.path().join("instances").join(id.as_str());
            assert_eq!(
                std::fs::read(game.join("config/preserved.txt")).unwrap(),
                b"published"
            );
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                receipt.after_manifest
            );
            {
                let pending = owner.pending.lock().unwrap();
                let batch = pending.get(&id).unwrap();
                assert!(!batch.restart_blocked);
                if acknowledgement == "UPDATE" {
                    assert!(
                        batch
                            .effects
                            .iter()
                            .any(|effect| matches!(effect, Effect::Committed))
                    );
                    assert!(
                        !batch
                            .effects
                            .iter()
                            .any(|effect| matches!(effect, Effect::Recovery(_)))
                    );
                    assert!(!receipt.native_settled);
                } else {
                    assert!(batch.effects.is_empty());
                    assert!(receipt.native_settled);
                }
            }
            owner
                .tasks
                .shutdown(std::time::Duration::from_secs(2))
                .await
                .unwrap();
            assert!(matches!(
                owner.release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap()),
                Err(MutationError::Pending)
            ));
            assert!(owner.has_unsettled_effects());
            assert_eq!(owner.pending.lock().unwrap().len(), 1);
            assert!(owner.directories.admit_content_settlement(&id).is_err());
            assert_eq!(recorded(), before);
            assert_eq!(
                std::fs::read(game.join("config/preserved.txt")).unwrap(),
                b"published"
            );
            assert_eq!(
                std::fs::read(game.join(MANIFEST_FILE)).unwrap(),
                receipt.after_manifest
            );
        }
    }

    async fn content_crash_restart(committed: bool, checkpoint_fault: Option<&str>) {
        use crate::library::LibraryId;
        use std::{io::Read, path::Path, process::Command, time::Duration};

        const ROOT: &str = "AXIAL_CONTENT_STAGED_CRASH_ROOT";
        const LIBRARY: &str = "AXIAL_CONTENT_STAGED_CRASH_LIBRARY";
        const EXIT: i32 = 42;
        const ORIGINAL: &[u8] = b"{\"schema_version\":3,\"entries\":[]}\n";
        const WITNESS: &str = "committed-tree.json";
        const WITNESS_BYTES: usize = 512 * 1024;
        let prefix = checkpoint_fault == Some("prefix");
        let parents = checkpoint_fault == Some("parents");
        let payload_path = if parents {
            "new-root/nested/staged.txt"
        } else {
            "config/staged.txt"
        };
        let override_path = if parents {
            "overrides/new-root/nested/staged.txt"
        } else {
            "overrides/config/staged.txt"
        };

        fn pending(owner: &ContentMutations, id: &InstanceId) -> Option<String> {
            owner
                .directories
                .registry()
                .storage()
                .read(|connection| {
                    connection
                        .query_row(
                            "SELECT receipt_json FROM content_batches WHERE instance_id=?1",
                            [id.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(StorageError::from)
                })
                .unwrap()
        }

        fn files(root: &Path) -> BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
            use std::io::Read;
            let mut pending = vec![root.to_path_buf()];
            let mut files = BTreeMap::new();
            let mut bytes = 0;
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(directory).unwrap() {
                    let path = entry.unwrap().path();
                    let relative = path.strip_prefix(root).unwrap().to_path_buf();
                    assert!(files.len() < 1024 && relative.components().count() <= 4);
                    let metadata = std::fs::symlink_metadata(&path).unwrap();
                    let value = if metadata.is_dir() {
                        pending.push(path);
                        None
                    } else {
                        assert!(metadata.is_file() && metadata.len() <= 512 * 1024);
                        let mut body = Vec::new();
                        std::fs::File::open(&path)
                            .unwrap()
                            .take(512 * 1024 + 1)
                            .read_to_end(&mut body)
                            .unwrap();
                        assert_eq!(body.len() as u64, metadata.len());
                        bytes += body.len();
                        assert!(bytes <= 1024 * 1024);
                        Some(body)
                    };
                    files.insert(relative, value);
                }
            }
            files
        }

        if let Some(root) = std::env::var_os(ROOT) {
            let root = std::path::PathBuf::from(root);
            assert_eq!(root.canonicalize().unwrap(), root);
            let library_id = LibraryId::parse(&std::env::var(LIBRARY).unwrap()).unwrap();
            let owner = open_crash_fixture(&root, library_id);
            let instances = InstanceService::new(owner.directories.clone(), owner.tasks.clone());
            let instance = instances
                .create(
                    CreateInstanceRequest {
                        name: "Content crash fixture".into(),
                        selection_id: "vanilla|1.21.4".into(),
                        ..Default::default()
                    },
                    CreateTarget {
                        selection_id: "vanilla|1.21.4".into(),
                        version_id: "1.21.4".into(),
                        minecraft_version: "1.21.4".into(),
                        loader_key: "vanilla".into(),
                    },
                )
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            let game = root.join("instances").join(instance.id.as_str());
            std::fs::write(game.join(MANIFEST_FILE), ORIGINAL).unwrap();
            std::fs::write(game.join("config/user.txt"), b"unrelated payload").unwrap();
            if parents {
                assert!(!game.join("new-root").exists());
                assert!(!game.join("new-root/nested").exists());
            }
            if !committed {
                let witness = serde_json::to_vec(&files(&game)).unwrap();
                assert!(witness.len() <= WITNESS_BYTES);
                std::fs::write(root.join(WITNESS), witness).unwrap();
            }
            let observer = owner.clone();
            let id = instance.id.clone();
            let reporting = owner.clone().with_progress(Arc::new(move |event| {
                if prefix {
                    if event.phase == "content_download" && event.current == 512 {
                        assert_eq!(event.total, 513);
                        let receipt: Receipt =
                            serde_json::from_str(&pending(&observer, &id).unwrap()).unwrap();
                        validate_receipt(&receipt).unwrap();
                        assert!(!receipt.native_settled && receipt.ready_checkpoint.is_some());
                        assert_eq!(receipt.before_manifest.as_deref(), Some(ORIGINAL));
                        assert_eq!(receipt.changes.len(), 513);
                        assert!(receipt.changes.iter().all(|change| matches!(
                            change.source,
                            Some(Source::PackOverride)
                        )
                            && !game.join(&change.path).exists()));
                        assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), ORIGINAL);
                        assert_eq!(
                            std::fs::read(game.join("config/user.txt")).unwrap(),
                            b"unrelated payload"
                        );
                        std::process::exit(EXIT);
                    }
                    return;
                }
                if event.phase != "content_commit"
                    || !(0..=i32::from(committed)).contains(&event.current)
                {
                    return;
                }
                assert_eq!(event.total, 1);
                assert!(!event.done);
                assert_eq!(
                    std::fs::read(game.join("config/user.txt")).unwrap(),
                    b"unrelated payload"
                );
                if event.current == 0 {
                    let receipt: Receipt =
                        serde_json::from_str(&pending(&observer, &id).unwrap()).unwrap();
                    validate_receipt(&receipt).unwrap();
                    assert!(!receipt.native_settled);
                    assert!(receipt.ready_checkpoint.is_some());
                    assert_eq!(receipt.before_manifest.as_deref(), Some(ORIGINAL));
                    assert_eq!(receipt.changes.len(), 1);
                    assert_eq!(receipt.changes[0].path, payload_path);
                    assert_eq!(receipt.changes[0].after, Some(Proof::bytes(b"new payload")));
                    assert!(matches!(
                        receipt.changes[0].source,
                        Some(Source::PackOverride)
                    ));
                    assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), ORIGINAL);
                    assert!(!game.join(payload_path).exists());
                    if parents {
                        if !game.join("new-root/nested").is_dir() {
                            eprintln!("content_commit(0) reached without publication parents");
                            std::process::exit(43);
                        }
                        let children = std::fs::read_dir(game.join("new-root"))
                            .unwrap()
                            .map(|entry| entry.unwrap().file_name())
                            .collect::<Vec<_>>();
                        assert_eq!(children, [std::ffi::OsString::from("nested")]);
                        assert_eq!(
                            std::fs::read_dir(game.join("new-root/nested"))
                                .unwrap()
                                .count(),
                            0
                        );
                        let proof = ManagedContentStagingCheckpoint::decode(
                            receipt.ready_checkpoint.as_deref().unwrap(),
                        )
                        .unwrap();
                        let proof: serde_json::Value =
                            serde_json::from_str(&proof.encode(MAX_RECEIPT_BYTES).unwrap())
                                .unwrap();
                        let created = proof["created_parents"]
                            .as_object()
                            .expect("sealed native parent proof");
                        assert_eq!(created.len(), 2);
                        assert!(
                            created.contains_key("new-root")
                                && created.contains_key("new-root/nested")
                        );
                        let private = game.join(proof["private_name"].as_str().unwrap());
                        let payloads = proof["payloads"].as_array().unwrap();
                        assert_eq!(payloads.len(), 1);
                        let stage = private.join("stage");
                        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 1);
                        assert_eq!(
                            std::fs::read_dir(private.join("backup")).unwrap().count(),
                            0
                        );
                        let staged = stage.join(payloads[0]["name"].as_str().unwrap());
                        let metadata = std::fs::symlink_metadata(&staged).unwrap();
                        assert!(
                            metadata.is_file() && metadata.len() == b"new payload".len() as u64
                        );
                        let mut body = Vec::new();
                        std::fs::File::open(staged)
                            .unwrap()
                            .take(b"new payload".len() as u64 + 1)
                            .read_to_end(&mut body)
                            .unwrap();
                        assert_eq!(body, b"new payload");
                    }
                    // The zero callback follows native stage(), before commit().
                    if committed {
                        return;
                    }
                } else {
                    // The one callback follows receipt deletion, before worker return.
                    assert!(pending(&observer, &id).is_none());
                    assert!(!observer.has_unsettled_effects());
                    assert!(matches!(
                        observer.directories.admit(&id),
                        Err(crate::instances::model::InstanceError::Busy)
                    ));
                    let mut witness = vec![0; WITNESS_BYTES];
                    let mut output = io::Cursor::new(witness.as_mut_slice());
                    serde_json::to_writer(&mut output, &files(&game)).unwrap();
                    let written = output.position() as usize;
                    std::fs::write(root.join(WITNESS), &witness[..written]).unwrap();
                }
                std::process::exit(EXIT);
            }));
            let result = reporting
                .install_pack(
                    &catalog(&owner),
                    &instance.id,
                    if prefix {
                        prefix_pack()
                    } else {
                        pack(&[], &[(override_path, b"new payload")])
                    },
                    true,
                )
                .unwrap()
                .join()
                .await;
            panic!("content did not reach crash boundary (committed={committed}): {result:?}");
        }

        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let library_id = LibraryId::new();
        let selector = match (committed, checkpoint_fault) {
            (true, None) => {
                "content::install::tests::hard_exit_after_content_metadata_commit_preserves_publication"
            }
            (false, None) => {
                "content::install::tests::hard_exit_after_content_staging_rolls_back_without_publication"
            }
            (false, Some("binding")) => {
                "content::install::tests::hard_exit_ready_checkpoint_binding_mismatch_preserves_fence"
            }
            (false, Some("missing")) => {
                "content::install::tests::hard_exit_ready_checkpoint_missing_preserves_fence"
            }
            (false, Some("delete")) => {
                "content::install::tests::hard_exit_ready_rollback_delete_refusal_retries_after_reopen"
            }
            (false, Some("foreign")) => {
                "content::install::tests::hard_exit_ready_checkpoint_foreign_public_file_allows_preserved_exit"
            }
            (false, Some("prefix")) => {
                "content::install::tests::hard_exit_after_512_content_payloads_rolls_back_recorded_prefix"
            }
            (false, Some("parents")) => {
                "content::install::tests::hard_exit_after_content_parent_preparation_rolls_back_empty_parents"
            }
            _ => unreachable!(),
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", selector, "--nocapture"])
            .env(ROOT, root.path())
            .env(LIBRARY, library_id.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let child_deadline_secs = if prefix { 60 } else { 20 };
        let deadline = std::time::Instant::now() + Duration::from_secs(child_deadline_secs);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out && output.status.code() == Some(EXIT),
            "crash boundary not reached (committed={committed}, prefix={prefix}, timed_out={timed_out}, deadline={child_deadline_secs}s): {:?}; {} {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let original_tree = {
            let mut witness = Vec::new();
            std::fs::File::open(root.path().join(WITNESS))
                .unwrap()
                .take(WITNESS_BYTES as u64 + 1)
                .read_to_end(&mut witness)
                .unwrap();
            assert!(witness.len() <= WITNESS_BYTES);
            serde_json::from_slice::<BTreeMap<std::path::PathBuf, Option<Vec<u8>>>>(&witness)
                .unwrap()
        };
        if !committed {
            let owner = open_crash_fixture(root.path(), library_id);
            let records = owner.directories.registry().list().unwrap();
            assert_eq!(records.len(), 1);
            let id = &records[0].instance.id;
            let receipt = pending(&owner, id).unwrap();
            let game = root.path().join("instances").join(id.as_str());
            let snapshot = files(&game);
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner
                .release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap())
                .unwrap();
            assert!(owner.has_unsettled_effects());
            assert!(owner.directories.admit(id).is_err());
            owner.directories.library().try_preserve().unwrap();
            assert_eq!(pending(&owner, id).as_deref(), Some(receipt.as_str()));
            assert_eq!(files(&game), snapshot);
        }
        if let Some(fault) = checkpoint_fault.filter(|_| !prefix && !parents) {
            let owner = open_crash_fixture(root.path(), library_id);
            let records = owner.directories.registry().list().unwrap();
            let id = &records[0].instance.id;
            let game = root.path().join("instances").join(id.as_str());
            let mut snapshot = files(&game);
            let raw = pending(&owner, id).unwrap();
            let mut changed: Receipt = serde_json::from_str(&raw).unwrap();
            let storage = owner.directories.registry().storage();
            if fault == "foreign" {
                std::fs::write(game.join("config/staged.txt"), b"foreign user payload").unwrap();
                snapshot = files(&game);
            } else if fault == "delete" {
                storage.transaction(|db| db.execute_batch(
                    "CREATE TRIGGER refuse_rollback_ack BEFORE DELETE ON content_batches BEGIN SELECT RAISE(IGNORE); END;"
                ).map_err(StorageError::from)).unwrap();
            } else {
                if fault == "binding" {
                    changed.operation_id = uuid::Uuid::new_v4().to_string();
                } else {
                    changed.ready_checkpoint = None;
                }
                validate_receipt(&changed).unwrap();
                storage.transaction(|db| db.execute(
                    "UPDATE content_batches SET operation_id=?1,receipt_json=?2 WHERE instance_id=?3 AND receipt_json=?4",
                    params![changed.operation_id, encoded_receipt(&changed).unwrap(), id.as_str(), raw],
                ).map_err(StorageError::from)).unwrap();
            }
            let expected = pending(&owner, id).unwrap();
            let result = owner.resume(id).unwrap().join().await.unwrap();
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner
                .release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap())
                .unwrap();
            owner.directories.library().try_preserve().unwrap();
            assert!(matches!(result, Err(MutationError::Pending)), "{result:?}");
            assert_eq!(pending(&owner, id).as_deref(), Some(expected.as_str()));
            assert!(owner.has_unsettled_effects());
            assert!(owner.directories.admit(id).is_err());
            assert_eq!(
                files(&game),
                if fault == "delete" {
                    original_tree.clone()
                } else {
                    snapshot
                }
            );
            if fault == "foreign" {
                assert_eq!(pending(&owner, id).as_deref(), Some(raw.as_str()));
                assert_eq!(
                    std::fs::read(game.join("config/staged.txt")).unwrap(),
                    b"foreign user payload"
                );
                return;
            }
            storage.transaction(|db| {
                if fault == "delete" {
                    db.execute_batch("DROP TRIGGER refuse_rollback_ack")?;
                } else {
                    let original: Receipt = serde_json::from_str(&raw).unwrap();
                    assert_eq!(db.execute(
                        "UPDATE content_batches SET operation_id=?1,receipt_json=?2 WHERE instance_id=?3 AND receipt_json=?4",
                        params![original.operation_id, raw, id.as_str(), expected],
                    )?, 1);
                }
                Ok::<_, StorageError>(())
            }).unwrap();
        }
        for attempt in 0..2 {
            let owner = open_crash_fixture(root.path(), library_id);
            let records = owner.directories.registry().list().unwrap();
            assert_eq!(records.len(), 1);
            let id = &records[0].instance.id;
            let game = root.path().join("instances").join(id.as_str());
            let receipt = pending(&owner, id);
            let snapshot = files(&game);
            if committed || attempt > 0 {
                assert!(receipt.is_none());
                assert_eq!(snapshot, original_tree);
            }
            assert_eq!(
                std::fs::read(game.join("config/user.txt")).unwrap(),
                b"unrelated payload"
            );
            let mut rollback = None;
            if committed {
                assert!(receipt.is_none());
                assert!(!owner.has_unsettled_effects());
                assert_eq!(
                    std::fs::read(game.join("config/staged.txt")).unwrap(),
                    b"new payload"
                );
                let expected = pack(&[], &[("overrides/config/staged.txt", b"new payload")]);
                let installed = owner.installed(id).unwrap();
                assert_eq!(installed.entries().len(), 1);
                let entry = installed.find(&expected.canonical_id).unwrap();
                assert_eq!(entry.provider(), ProviderId::Modrinth);
                assert_eq!(entry.project_id(), "fixture-pack");
                assert_eq!(entry.version_id(), expected.version_id);
                assert_eq!(entry.kind(), ContentKind::Modpack);
                assert!(entry.enabled());
                assert_eq!(entry.title(), Some("Pack fixture"));
                assert_eq!(
                    entry.pack_installation(),
                    Some(&PackInstallation {
                        fingerprint: expected.archive.fingerprint().into(),
                        files: vec![PackInstalledFile {
                            path: "config/staged.txt".into(),
                            size: b"new payload".len() as u64,
                            sha512: Proof::bytes(b"new payload").sha512,
                        }],
                    })
                );
                assert_eq!(
                    installed.encode_managed().unwrap(),
                    std::fs::read(game.join(MANIFEST_FILE)).unwrap()
                );
                owner
                    .directories
                    .admit_read(id)
                    .unwrap()
                    .validate_current()
                    .unwrap();
                let admitted = owner.directories.admit(id).unwrap();
                assert!(
                    owner
                        .installed_pack_admitted(
                            &admitted,
                            &expected.canonical_id,
                            &expected.version_id,
                            expected.archive.fingerprint()
                        )
                        .unwrap()
                );
            } else if attempt == 0 {
                assert_eq!(std::fs::read(game.join(MANIFEST_FILE)).unwrap(), ORIGINAL);
                assert!(!game.join(payload_path).exists());
                assert!(matches!(
                    owner.installed(id),
                    Err(MutationError::Unavailable)
                ));
                assert!(matches!(
                    owner.directories.admit_read(id),
                    Err(crate::instances::model::InstanceError::Busy)
                ));
                assert!(matches!(
                    owner.directories.admit(id),
                    Err(crate::instances::model::InstanceError::Busy)
                ));
                let admitted = owner
                    .directories
                    .admit_content_settlement(id)
                    .expect("only settlement admission may bypass the content fence");
                let record: Receipt = serde_json::from_str(receipt.as_deref().unwrap()).unwrap();
                validate_receipt(&record).unwrap();
                validate_instance(&admitted, &record).unwrap();
                assert!(!record.native_settled);
                drop(admitted);
                let foreign = TaskOwner::new(1).unwrap();
                foreign.try_close_idle().unwrap();
                assert!(matches!(
                    owner.release_shutdown_admissions(&foreign.shutdown_receipt().unwrap()),
                    Err(MutationError::Unavailable)
                ));
                rollback = Some(owner.resume(id).unwrap().join().await.unwrap());
            } else {
                assert!(!owner.has_unsettled_effects());
                owner
                    .directories
                    .admit(id)
                    .unwrap()
                    .validate_current()
                    .unwrap();
            }
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner
                .release_shutdown_admissions(&owner.tasks.shutdown_receipt().unwrap())
                .unwrap();
            assert!(owner.pending.lock().unwrap().is_empty());
            owner.directories.library().try_preserve().unwrap();
            if let Some(result) = rollback {
                assert!(
                    matches!(result, Err(MutationError::Cancelled)),
                    "Ready-stage recovery must prove rollback, not publication: {result:?}"
                );
            }
            assert!(pending(&owner, id).is_none());
            assert_eq!(files(&game), original_tree);
        }
        if !committed {
            let owner = open_crash_fixture(root.path(), library_id);
            let records = owner.directories.registry().list().unwrap();
            assert_eq!(records.len(), 1);
            let id = &records[0].instance.id;
            let installed = owner
                .install_pack(
                    &catalog(&owner),
                    id,
                    if prefix {
                        prefix_pack()
                    } else {
                        pack(&[], &[(override_path, b"new payload")])
                    },
                    true,
                )
                .unwrap()
                .join()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&installed.instance_id, id);
            assert_eq!(installed.status, "complete");
            assert!(!owner.has_unsettled_effects());
            let game = root.path().join("instances").join(id.as_str());
            assert_eq!(
                std::fs::read(game.join("config/user.txt")).unwrap(),
                b"unrelated payload"
            );
            if prefix {
                for index in 0..513 {
                    assert_eq!(
                        std::fs::read(game.join(format!("config/prefix-{index:04}.txt"))).unwrap(),
                        b"new payload"
                    );
                }
            } else {
                assert_eq!(
                    std::fs::read(game.join(payload_path)).unwrap(),
                    b"new payload"
                );
            }
            owner.tasks.shutdown(Duration::from_secs(2)).await.unwrap();
            owner.directories.library().try_preserve().unwrap();
        }
    }

    #[tokio::test]
    async fn identified_pack_member_remains_manageable_when_display_metadata_fails() {
        let (root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let pack = pack(
            &[
                ("resourcepacks/known.zip", b"known"),
                ("resourcepacks/unknown.zip", b"unknown"),
            ],
            &[("overrides/config/settings.txt", b"config")],
        );
        let plan = pack.archive.plan_all(true).unwrap();
        let mut receipt = pack_record(&instance, &pack);
        let (service, provider) = identity_provider(&Proof::bytes(b"known").sha512, true).await;
        let entries = identify_pack_members(
            &service,
            &plan,
            &receipt.changes,
            &pack.canonical_id,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        provider.await.unwrap();
        assert_eq!(entries.len(), 1);
        let member = entries[0].canonical_id().clone();
        assert_eq!(entries[0].title(), Some("Identified fixture"));
        let mut after = ContentManifest::decode_managed(Some(&receipt.after_manifest)).unwrap();
        after.try_upsert_batch(entries).unwrap();
        receipt.after_manifest = after.encode_managed().unwrap();
        validate_receipt(&receipt).unwrap();
        owner.persist_receipt(&receipt).unwrap();
        owner
            .apply_pack(
                instance,
                receipt,
                HashMap::from([
                    ("resourcepacks/known.zip".into(), b"known".to_vec()),
                    ("resourcepacks/unknown.zip".into(), b"unknown".to_vec()),
                ]),
                Some(&plan),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(owner.installed(&id).unwrap().find(&member).is_some());
        owner
            .remove(&id, &member)
            .unwrap()
            .join()
            .await
            .unwrap()
            .unwrap();
        let directory = root.path().join("instances").join(id.as_str());
        assert!(!directory.join("resourcepacks/known.zip").exists());
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/unknown.zip")).unwrap(),
            b"unknown"
        );
        assert_eq!(
            std::fs::read(directory.join("config/settings.txt")).unwrap(),
            b"config"
        );
        assert!(!owner.has_unsettled_effects());
    }

    #[tokio::test]
    async fn pack_updates_renamed_members_and_restores_absent_members_without_pruning() {
        for (old_name, variants) in [
            ("old.zip", vec!["old.zip", "old.zip.disabled"]),
            ("old.zip", vec![]),
            ("known.zip", vec![]),
        ] {
            let (root, owner, id) = fixture().await;
            let directory = root.path().join("instances").join(id.as_str());
            let old = known_member(old_name, b"old");
            let mut before = ContentManifest::default();
            before
                .try_upsert_batch(vec![old.clone(), entry(b"unrelated")])
                .unwrap();
            std::fs::write(
                directory.join(MANIFEST_FILE),
                before.encode_managed().unwrap(),
            )
            .unwrap();
            std::fs::write(directory.join("resourcepacks/pack.zip"), b"unrelated").unwrap();
            for variant in &variants {
                std::fs::write(directory.join("resourcepacks").join(variant), b"old").unwrap();
            }
            let instance = owner.directories.admit(&id).unwrap();
            let pack = pack(
                &[("resourcepacks/known.zip", b"known")],
                &[("overrides/config/settings.txt", b"config")],
            );
            let plan = pack.archive.plan_all(true).unwrap();
            let mut receipt = pack_record(&instance, &pack);
            let (service, provider) = identity_provider(&Proof::bytes(b"known").sha512, true).await;
            let members = identify_pack_members(
                &service,
                &plan,
                &receipt.changes,
                &pack.canonical_id,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            provider.await.unwrap();
            let mut after = ContentManifest::decode_managed(Some(&receipt.after_manifest)).unwrap();
            after.try_upsert_batch(members).unwrap();
            receipt.after_manifest = after.encode_managed().unwrap();
            for variant in &variants {
                receipt.changes.push(Change {
                    path: format!("resourcepacks/{variant}"),
                    before: Some(Proof::bytes(b"old")),
                    after: None,
                    source: None,
                });
            }
            validate_receipt(&receipt).unwrap();
            preflight(instance.game_directory(), &receipt, false).unwrap();
            owner.persist_receipt(&receipt).unwrap();
            let result = owner
                .apply_pack(
                    instance,
                    receipt,
                    HashMap::from([("resourcepacks/known.zip".into(), b"known".to_vec())]),
                    Some(&plan),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(result.changed_files, 2 + variants.len());
            assert_eq!(
                std::fs::read(directory.join("resourcepacks/known.zip")).unwrap(),
                b"known"
            );
            assert_eq!(
                std::fs::read(directory.join("config/settings.txt")).unwrap(),
                b"config"
            );
            assert!(!directory.join("resourcepacks/old.zip").exists());
            assert!(!directory.join("resourcepacks/old.zip.disabled").exists());
            assert_eq!(
                std::fs::read(directory.join("resourcepacks/pack.zip")).unwrap(),
                b"unrelated"
            );
            let installed = owner.installed(&id).unwrap();
            let member = installed.find(old.canonical_id()).unwrap();
            assert_eq!(member.version_id(), "known-v1");
            assert_eq!(member.managed_filename().unwrap().as_str(), "known.zip");
            assert_eq!(
                member.sha512(),
                Some(Proof::bytes(b"known").sha512.as_str())
            );
            assert!(installed.find(entry(b"unrelated").canonical_id()).is_some());
            assert!(!owner.has_unsettled_effects());
        }
    }

    fn known_member(filename: &str, bytes: &[u8]) -> ManifestEntry {
        ManifestEntry::managed(
            CanonicalId::for_project(ProviderId::Modrinth, "known"),
            ProviderId::Modrinth,
            "known".into(),
            "old-version".into(),
            ContentKind::ResourcePack,
            &FileRef {
                filename: filename.into(),
                url: "https://cdn.example.com/old.zip".into(),
                size: Some(bytes.len() as u64),
                sha512: Some(Proof::bytes(bytes).sha512),
                sha1: None,
                primary: true,
            },
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pack_update_refuses_modified_old_variants_before_any_effect() {
        for variant in ["old.zip", "old.zip.disabled"] {
            let (root, owner, id) = fixture().await;
            let directory = root.path().join("instances").join(id.as_str());
            let mut before = ContentManifest::default();
            before.try_upsert(known_member("old.zip", b"old")).unwrap();
            let manifest = before.encode_managed().unwrap();
            std::fs::write(directory.join(MANIFEST_FILE), &manifest).unwrap();
            std::fs::write(directory.join("resourcepacks").join(variant), b"user edit").unwrap();
            let pack = pack(
                &[("resourcepacks/known.zip", b"known")],
                &[("overrides/config/settings.txt", b"config")],
            );
            let (service, provider) = identity_provider(&Proof::bytes(b"known").sha512, true).await;
            assert!(matches!(
                owner
                    .install_pack(&service, &id, pack, true)
                    .unwrap()
                    .join()
                    .await
                    .unwrap(),
                Err(MutationError::Conflict)
            ));
            provider.await.unwrap();
            assert_eq!(
                std::fs::read(directory.join(MANIFEST_FILE)).unwrap(),
                manifest
            );
            assert_eq!(
                std::fs::read(directory.join("resourcepacks").join(variant)).unwrap(),
                b"user edit"
            );
            assert!(!directory.join("resourcepacks/known.zip").exists());
            assert!(!directory.join("config/settings.txt").exists());
            assert!(!owner.has_unsettled_effects());
        }
    }

    #[tokio::test]
    async fn pack_update_guards_absent_old_variants_and_rejects_unrelated_deletion_authority() {
        let (root, owner, id) = fixture().await;
        let directory = root.path().join("instances").join(id.as_str());
        let mut before = ContentManifest::default();
        before
            .try_upsert_batch(vec![known_member("old.zip", b"old"), entry(b"unrelated")])
            .unwrap();
        std::fs::write(
            directory.join(MANIFEST_FILE),
            before.encode_managed().unwrap(),
        )
        .unwrap();
        let instance = owner.directories.admit(&id).unwrap();
        let pack = pack(&[("resourcepacks/known.zip", b"known")], &[]);
        let mut receipt = pack_record(&instance, &pack);
        let mut after = ContentManifest::decode_managed(Some(&receipt.after_manifest)).unwrap();
        after
            .try_upsert(known_member("known.zip", b"known"))
            .unwrap();
        receipt.after_manifest = after.encode_managed().unwrap();
        validate_receipt(&receipt).unwrap();
        preflight(instance.game_directory(), &receipt, false).unwrap();
        assert_eq!(
            removed_absence_preconditions(&receipt).unwrap(),
            ["resourcepacks/old.zip", "resourcepacks/old.zip.disabled",]
        );
        receipt.changes.push(Change {
            path: "resourcepacks/pack.zip".into(),
            before: Some(Proof::bytes(b"unrelated")),
            after: None,
            source: None,
        });
        assert!(matches!(
            validate_receipt(&receipt),
            Err(MutationError::Changed)
        ));
        receipt.changes.pop();
        std::fs::write(
            directory.join("resourcepacks/old.zip.disabled"),
            b"reappeared",
        )
        .unwrap();
        assert!(matches!(
            preflight(instance.game_directory(), &receipt, false),
            Err(MutationError::Conflict)
        ));
        let mut effects = Vec::new();
        assert!(
            apply_streamed(
                &owner,
                &instance,
                &mut receipt,
                &mut HashMap::from([("resourcepacks/known.zip".into(), b"known".to_vec())]),
                None,
                &mut effects,
                &CancellationToken::new(),
                None,
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read(directory.join("resourcepacks/old.zip.disabled")).unwrap(),
            b"reappeared"
        );
        assert!(!directory.join("resourcepacks/known.zip").exists());
    }

    #[tokio::test]
    async fn repeated_identified_pack_member_is_refused_before_any_effect() {
        let (_root, owner, id) = fixture().await;
        let instance = owner.directories.admit(&id).unwrap();
        let pack = pack(
            &[
                ("resourcepacks/first.zip", b"known"),
                ("resourcepacks/second.zip", b"known"),
            ],
            &[],
        );
        let receipt = pack_record(&instance, &pack);
        let (service, provider) = identity_provider(&Proof::bytes(b"known").sha512, false).await;
        assert!(matches!(
            identify_pack_members(
                &service,
                &pack.archive.plan_all(true).unwrap(),
                &receipt.changes,
                &pack.canonical_id,
                &CancellationToken::new()
            )
            .await,
            Err(MutationError::Conflict)
        ));
        provider.await.unwrap();
        assert!(!owner.has_unsettled_effects());
        assert!(
            owner
                .installed_pack_admitted(
                    &instance,
                    &pack.canonical_id,
                    &pack.version_id,
                    pack.archive.fingerprint()
                )
                .is_ok_and(|installed| !installed)
        );
    }
}
