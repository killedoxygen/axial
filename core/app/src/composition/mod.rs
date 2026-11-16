//! Recorded managed compositions: the exact managed files of an instance,
//! recovery of the operations an earlier run of this application recorded,
//! launch proof that a composition is absent or settled, and duplication of
//! settled managed files. Nothing here plans or starts new composition work.

mod artifact;
#[cfg(test)]
mod authority_tests;
pub(crate) mod duplicate;
mod health;
pub mod model;
pub(crate) mod plan;
pub mod recovery;
pub mod rollback;
pub(crate) mod state;
pub(crate) mod storage;
#[cfg(test)]
mod tests;

pub use recovery::{CompositionError, CompositionService, CompositionSettled};

/// The retired rules cache stays part of this application's recorded
/// schema; nothing reads or writes it.
pub const RULES_MIGRATION: crate::storage::Migration = crate::storage::Migration {
    id: "performance_rules.v1",
    sql: "CREATE TABLE performance_rules (singleton INTEGER PRIMARY KEY CHECK(singleton=1), snapshot BLOB NOT NULL CHECK(length(snapshot)<=1048576)) STRICT;",
};

/// Fixture producers fetch managed artifacts through the public transfer policy.
#[cfg(any(test, feature = "test-support"))]
pub fn public_transfer_resolver() -> ManagedArtifactTransferResolver {
    ManagedArtifactTransferResolver::new(
        |url| async move {
            let origin = axial_minecraft::download::TransferOrigin::from_url(&url)
                .map_err(|_| std::io::Error::other("managed artifact origin was not admitted"))?;
            crate::network::pinned_public_transfer_client(origin, &url).await
        },
        crate::network::managed_transfer_retry_policy(),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use artifact::ManagedArtifactTransferRecovery;
#[cfg(any(test, feature = "test-support"))]
pub use artifact::ManagedArtifactTransferResolver;
pub use plan::{
    ManagedArtifactPin, ManagedCompositionInstallPlan, ManagedDependencyEdge,
    ManagedInstallPlanError,
};
pub use state::{
    ManagedDuplicateFile, ManagedDuplicatePayload, ManagedRollbackOutcome, RollbackSnapshotSummary,
    RollbackSnapshotTarget, StateError,
};
pub use storage::ManagedInstanceEffectAuthority;

#[cfg(any(test, feature = "test-support"))]
use artifact::{
    ManagedArtifactStage, StagedManagedArtifact, installed_graph_from_plan, stage_managed_graph,
    state_from_plan,
};
use axial_fs::{Directory, DirectoryListingState, EntryKind};
use axial_minecraft::portable_path::{PortableFileName, PortablePathKey};
use axial_resource::{
    PhysicalIoClass, PhysicalWorkError, PhysicalWorkRequest, process_physical_work,
};
#[cfg(any(test, feature = "test-support"))]
use model::InstalledMod;
use model::{CompositionPlan, CompositionState};
#[cfg(any(test, feature = "test-support"))]
use state::{
    RollbackRestoreError, load_rollback_snapshot, load_rollback_snapshot_async,
    load_rollback_snapshot_by_id_async, load_state, remove_state, restore_rollback_snapshot,
    restore_rollback_snapshot_classified_async, save_absent_rollback_snapshot_async,
    save_rollback_snapshot, save_rollback_snapshot_async, save_state,
    settle_managed_artifact_removal, stage_managed_artifact_removal,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use storage::{ManagedStorageDirectory, WeakManagedInstanceEffectAuthority};

pub(crate) const MANAGED_ARTIFACT_MAX_BYTES: u64 = 512 << 20;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("failed to access performance state: {0}")]
    State(#[from] StateError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("managed artifact transfer failed")]
    Transfer,
    #[error("no performance rollback snapshot available")]
    NoRollbackSnapshot,
    #[error("performance rollback snapshot not found")]
    RollbackSnapshotNotFound,
}

#[derive(Clone)]
pub struct ManagedCompositionAuthority {
    instances_root_directory: Arc<Directory>,
    instance_effect_authorities: Arc<Mutex<HashMap<String, WeakManagedInstanceEffectAuthority>>>,
}

#[derive(Clone, Debug)]
pub struct ManagedInstanceIdentity {
    instance_id: Arc<str>,
    admitted_directory: Directory,
}

impl ManagedInstanceIdentity {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[derive(Clone, Debug)]
pub struct ManagedCompositionInspection {
    pub state: Option<CompositionState>,
    pub health: model::BundleHealth,
    pub warnings: Vec<String>,
    pub installed_mod_evidence: Vec<String>,
    pub rollback_snapshots: Vec<RollbackSnapshotSummary>,
}

#[derive(Clone, Debug)]
pub struct ManagedResolvedInspection {
    pub plan: CompositionPlan,
    pub inspection: ManagedCompositionInspection,
}

#[derive(Clone, Debug)]
pub struct ManagedInstallExecutionOutcome {
    state: CompositionState,
    target_changed: bool,
}

impl ManagedInstallExecutionOutcome {
    pub fn into_state(self) -> CompositionState {
        self.state
    }

    pub fn target_changed(&self) -> bool {
        self.target_changed
    }
}

#[derive(Debug)]
pub enum ManagedInstallExecutionError<BeforeTargetEffectError> {
    Mutation(ManagedMutationError),
    /// The caller's checkpoint refused the target effect; staged candidates were settled.
    BeforeTargetEffect(BeforeTargetEffectError),
}

impl<BeforeTargetEffectError> From<ManagedMutationError>
    for ManagedInstallExecutionError<BeforeTargetEffectError>
{
    fn from(error: ManagedMutationError) -> Self {
        Self::Mutation(error)
    }
}

pub struct ManagedArtifactWitnessProof {
    filename: PortablePathKey,
}

impl ManagedArtifactWitnessProof {
    /// Ownership protection does not disappear when a file is modified or
    /// renamed to its disabled spelling. Callers must not mutate either alias.
    pub fn protects_filename(&self, filename: &str) -> bool {
        let enabled = filename
            .rsplit_once('.')
            .filter(|(_, extension)| extension.eq_ignore_ascii_case("disabled"))
            .map_or(filename, |(enabled, _)| enabled);
        PortableFileName::new_exact(enabled).is_ok_and(|filename| filename.key() == self.filename)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedMutationError {
    #[error("managed composition operation was definitely rejected: {0}")]
    Definite(#[source] InstallError),
    #[error("{0}")]
    Indeterminate(ManagedIndeterminate),
}

#[derive(Debug, thiserror::Error)]
#[error("managed composition outcome is indeterminate after {operation}: {source}")]
pub struct ManagedIndeterminate {
    operation: &'static str,
    #[source]
    source: ManagedIndeterminateSource,
}

#[derive(Debug, thiserror::Error)]
enum ManagedIndeterminateSource {
    #[error("{0}")]
    Install(#[source] InstallError),
    #[error("owned managed composition task stopped before reporting completion")]
    TaskStopped,
    #[error("managed composition identity requires exact reconciliation")]
    ReconciliationRequired,
}

impl ManagedMutationError {
    fn definite(error: impl Into<InstallError>) -> Self {
        Self::Definite(error.into())
    }

    fn indeterminate(operation: &'static str, error: impl Into<InstallError>) -> Self {
        Self::Indeterminate(ManagedIndeterminate {
            operation,
            source: ManagedIndeterminateSource::Install(error.into()),
        })
    }

    fn task_stopped(operation: &'static str) -> Self {
        Self::Indeterminate(ManagedIndeterminate {
            operation,
            source: ManagedIndeterminateSource::TaskStopped,
        })
    }

    pub fn reconciliation_required(operation: &'static str) -> Self {
        Self::Indeterminate(ManagedIndeterminate {
            operation,
            source: ManagedIndeterminateSource::ReconciliationRequired,
        })
    }
}

impl ManagedCompositionAuthority {
    /// The identifier labels effects; the already admitted directory grants access.
    /// Callers retain registry admission until every effect and receipt settles.
    pub fn bind(
        instance_id: &str,
        directory: Directory,
    ) -> Result<(Self, ManagedInstanceIdentity), std::io::Error> {
        directory.identity()?;
        axial_fs::LeafName::new(instance_id).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid managed instance label",
            )
        })?;
        Ok((
            Self {
                instances_root_directory: Arc::new(directory.clone()),
                instance_effect_authorities: Arc::new(Mutex::new(HashMap::new())),
            },
            ManagedInstanceIdentity {
                instance_id: Arc::from(instance_id),
                admitted_directory: directory,
            },
        ))
    }

    pub async fn bind_instance_effect_authority(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<ManagedInstanceEffectAuthority, ManagedMutationError> {
        let instance = self.open_instance_directory(identity).await?;
        let anchor_instance = instance.clone();
        let instance_anchor = run_managed_blocking(PhysicalIoClass::Metadata, move || {
            anchor_instance.identity()
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("bind_effect_authority"))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))?;
        {
            let mut authorities = self
                .instance_effect_authorities
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            authorities.retain(|_, effects| effects.upgrade().is_some());
            if let Some(effects) = authorities
                .get(identity.instance_id())
                .and_then(|effects| effects.upgrade())
            {
                return require_effect_anchor(effects, instance_anchor);
            }
        }
        let candidate = run_managed_blocking(PhysicalIoClass::Metadata, move || {
            ManagedInstanceEffectAuthority::bind(&instance)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("bind_effect_authority"))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))?;
        let mut authorities = self
            .instance_effect_authorities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        authorities.retain(|_, effects| effects.upgrade().is_some());
        if let Some(effects) = authorities
            .get(identity.instance_id())
            .and_then(|effects| effects.upgrade())
        {
            return require_effect_anchor(effects, instance_anchor);
        }
        authorities.insert(identity.instance_id().to_string(), candidate.downgrade());
        Ok(candidate)
    }

    pub async fn composition_managed_witness_proofs(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<Vec<ManagedArtifactWitnessProof>, ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) =
            open_mods_if_present(instance, "composition_managed_witness_proofs").await?
        else {
            return Ok(Vec::new());
        };
        run_managed_blocking(PhysicalIoClass::Read, move || {
            let state =
                state::load_state_admitted(&mods).map_err(ManagedMutationError::definite)?;
            let mut proofs = state
                .into_iter()
                .flat_map(|state| state.installed_mods)
                .map(|installed| ManagedArtifactWitnessProof {
                    filename: PortableFileName::new_exact(&installed.filename)
                        .expect("admitted performance state has portable filenames")
                        .key(),
                })
                .collect::<Vec<_>>();
            proofs.sort_by(|left, right| left.filename.cmp(&right.filename));
            proofs.dedup_by(|left, right| left.filename == right.filename);
            Ok(proofs)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("composition_managed_witness_proofs"))?
    }

    pub async fn recover_and_inspect(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let settle_effects = effects.clone();
        run_managed_blocking(PhysicalIoClass::Heavy, move || {
            settle_effects.settle()?;
            settle_effects.require_settled()
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("recover_effects"))?
        .map_err(|error| ManagedMutationError::indeterminate("recover_effects", error))?;
        let inspection =
            if let Some(mods) = open_mods_if_present(instance.clone(), "recover").await? {
                let recovery_mods = mods.clone();
                run_managed_blocking(PhysicalIoClass::Heavy, move || {
                    state::recover_managed_storage(&recovery_mods)
                })
                .await
                .map_err(|_| ManagedMutationError::task_stopped("recover"))?
                .map_err(|error| classify_state_reconciliation_error("recover", error))?;
                run_managed_blocking(PhysicalIoClass::Read, move || recovered_inspection(mods))
                    .await
                    .map_err(|_| ManagedMutationError::task_stopped("recover"))??
            } else {
                absent_inspection(None, None)
            };
        let final_effects = effects.clone();
        run_managed_blocking(PhysicalIoClass::Metadata, move || {
            final_effects.require_settled()
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("recover_effects"))?
        .map_err(|error| ManagedMutationError::indeterminate("recover_effects", error))?;
        Ok(inspection)
    }

    /// Read-only proof that the instance's composition is absent or fully
    /// settled: its effect owner has nothing unsettled, no publication or
    /// reconciliation obligation remains, and every managed artifact matches
    /// its recorded size and SHA-512. Writes nothing; any unproven state is
    /// an error for the caller's recovery path.
    pub async fn prove_settled(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let settled = effects.clone();
        run_managed_blocking(PhysicalIoClass::Metadata, move || settled.require_settled())
            .await
            .map_err(|_| ManagedMutationError::task_stopped("prove_settled"))?
            .map_err(|error| ManagedMutationError::indeterminate("prove_settled", error))?;
        match open_mods_if_present(instance, "prove_settled").await? {
            None => Ok(absent_inspection(None, None)),
            Some(mods) => {
                run_managed_blocking(PhysicalIoClass::Read, move || recovered_inspection(mods))
                    .await
                    .map_err(|_| ManagedMutationError::task_stopped("prove_settled"))?
            }
        }
    }

    /// Fixture preparation: production never starts composition work.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn ensure_installed<
        BeforeTargetEffect,
        BeforeTargetEffectFuture,
        BeforeTargetEffectError,
    >(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
        plan: &ManagedCompositionInstallPlan,
        resolver: ManagedArtifactTransferResolver,
        before_target_effect: BeforeTargetEffect,
    ) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
    where
        BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture,
        BeforeTargetEffectFuture: std::future::Future<Output = Result<(), BeforeTargetEffectError>>,
    {
        let instance = self.validate_identity(identity, effects).await?;
        install_graph(plan, resolver, &instance, before_target_effect).await
    }

    /// Fixture preparation: production never starts composition work.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn remove_managed(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<(), ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) = open_mods_if_present(instance, "remove").await? else {
            return Ok(());
        };
        run_managed_blocking(PhysicalIoClass::Heavy, move || {
            remove_managed_transaction(&mods)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("remove"))?
        .map_err(|error| classify_install_reconciliation_error("remove", error))
    }

    #[cfg(test)]
    pub async fn rollback_managed(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) = open_mods_if_present(instance, "rollback_preflight").await? else {
            return Err(ManagedMutationError::definite(
                InstallError::NoRollbackSnapshot,
            ));
        };
        let snapshot = load_rollback_snapshot_async(&mods)
            .await
            .map_err(|error| classify_state_reconciliation_error("rollback_preflight", error))?
            .ok_or_else(|| ManagedMutationError::definite(InstallError::NoRollbackSnapshot))?;
        restore_rollback_snapshot_classified_async(&mods, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }

    /// Fixture preparation: production never starts composition work.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn rollback_managed_snapshot(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
        snapshot_id: &str,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) = open_mods_if_present(instance, "rollback_preflight").await? else {
            return Err(ManagedMutationError::definite(
                InstallError::RollbackSnapshotNotFound,
            ));
        };
        state::validate_rollback_snapshot_id(snapshot_id)
            .map_err(ManagedMutationError::definite)?;
        let snapshot = load_rollback_snapshot_by_id_async(&mods, snapshot_id)
            .await
            .map_err(|error| classify_state_reconciliation_error("rollback_preflight", error))?
            .ok_or_else(|| {
                ManagedMutationError::definite(InstallError::RollbackSnapshotNotFound)
            })?;
        restore_rollback_snapshot_classified_async(&mods, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }

    async fn validate_identity(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<ManagedStorageDirectory, ManagedMutationError> {
        let instance = self.open_instance_directory(identity).await?;
        ManagedStorageDirectory::bind_instance_root(instance, effects.clone())
            .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
    }

    async fn open_instance_directory(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<Directory, ManagedMutationError> {
        let directory = &identity.admitted_directory;
        let current = directory
            .identity()
            .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))?;
        let bound = self
            .instances_root_directory
            .identity()
            .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))?;
        if current != bound {
            return Err(ManagedMutationError::reconciliation_required(
                "admitted_instance_mismatch",
            ));
        }
        Ok(directory.clone())
    }
}

fn require_effect_anchor(
    effects: ManagedInstanceEffectAuthority,
    current: axial_fs::DirectoryIdentity,
) -> Result<ManagedInstanceEffectAuthority, ManagedMutationError> {
    if effects.anchor_identity() == current {
        Ok(effects)
    } else {
        Err(ManagedMutationError::definite(InstallError::Io(
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed composition instance changed while its effect authority was live",
            ),
        )))
    }
}

async fn open_mods_if_present(
    instance: ManagedStorageDirectory,
    operation: &'static str,
) -> Result<Option<ManagedStorageDirectory>, ManagedMutationError> {
    run_managed_blocking(PhysicalIoClass::Metadata, move || {
        instance.open_child("mods")
    })
    .await
    .map_err(|_| ManagedMutationError::task_stopped(operation))?
    .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
}

async fn run_managed_blocking<T, Work>(
    io: PhysicalIoClass,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    process_physical_work()
        .admit(PhysicalWorkRequest::foreground(io, 0))
        .await?
        .run(move |_| work())
        .await
}

fn absent_inspection(
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
) -> ManagedCompositionInspection {
    let (health, warnings) = health::derive_health(None, plan, expected_game_version, None);
    ManagedCompositionInspection {
        state: None,
        health,
        warnings,
        installed_mod_evidence: Vec::new(),
        rollback_snapshots: Vec::new(),
    }
}

fn recovered_inspection(
    instance_mods: ManagedStorageDirectory,
) -> Result<ManagedCompositionInspection, ManagedMutationError> {
    let state = state::load_state_admitted(&instance_mods)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    state::prove_managed_storage_recovered(&instance_mods, state.as_ref())
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    let (health, warnings) =
        health::derive_health(state.as_ref(), None, None, Some(&instance_mods));
    let installed_mod_evidence = installed_mod_evidence(&instance_mods, state.as_ref())
        .map_err(ManagedMutationError::definite)?;
    let rollback_snapshots = state::list_rollback_snapshots_admitted(&instance_mods)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    Ok(ManagedCompositionInspection {
        state,
        health,
        warnings,
        installed_mod_evidence,
        rollback_snapshots,
    })
}

fn installed_mod_evidence(
    instance_mods: &ManagedStorageDirectory,
    state: Option<&CompositionState>,
) -> Result<Vec<String>, InstallError> {
    let mut evidence = std::collections::BTreeSet::new();
    for installed in state.into_iter().flat_map(|state| &state.installed_mods) {
        add_mod_evidence(&mut evidence, &installed.project_id);
        add_mod_evidence(&mut evidence, &installed.filename);
    }
    let listing = instance_mods
        .entries(state::RECOVERY_ENTRY_LIMIT + 1)
        .map_err(InstallError::Io)?;
    if listing.state() != DirectoryListingState::Complete
        || listing.entries().len() > state::RECOVERY_ENTRY_LIMIT
    {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed composition evidence exceeds the directory entry limit",
        )));
    }
    for entry in listing.entries() {
        if entry.kind() != EntryKind::File {
            continue;
        }
        let Some(filename) = entry.utf8_name() else {
            continue;
        };
        let Some((stem, extension)) = filename.rsplit_once('.') else {
            continue;
        };
        if !extension.eq_ignore_ascii_case("jar") || stem.is_empty() {
            continue;
        }
        add_mod_evidence(&mut evidence, stem);
    }
    Ok(evidence.into_iter().collect())
}

fn add_mod_evidence(evidence: &mut std::collections::BTreeSet<String>, raw: &str) {
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return;
    }
    evidence.insert(normalized.clone());
    let mut prefix = String::new();
    for token in normalized
        .split(|value: char| !value.is_ascii_alphanumeric())
        .filter(|value| !value.is_empty())
    {
        let versionish = token.strip_prefix("mc").is_some_and(starts_with_digit)
            || token.strip_prefix('v').is_some_and(starts_with_digit)
            || starts_with_digit(token);
        if versionish {
            break;
        }
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(token);
        evidence.insert(prefix.clone());
    }
}

fn starts_with_digit(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|value| value.is_ascii_digit())
}

#[cfg(any(test, feature = "test-support"))]
async fn install_graph<BeforeTargetEffect, BeforeTargetEffectFuture, BeforeTargetEffectError>(
    plan: &ManagedCompositionInstallPlan,
    resolver: ManagedArtifactTransferResolver,
    instance: &ManagedStorageDirectory,
    before_target_effect: BeforeTargetEffect,
) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
where
    BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture,
    BeforeTargetEffectFuture: std::future::Future<Output = Result<(), BeforeTargetEffectError>>,
{
    let existing_mods = open_mods_if_present(instance.clone(), "install_preflight").await?;
    let selection = match existing_mods.as_ref() {
        Some(mods) => managed_stage_selection(mods, plan),
        None => Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        }),
    }
    .map_err(ManagedMutationError::definite)?;
    if let Some(state) = selection.exact_state {
        return Ok(ManagedInstallExecutionOutcome {
            state,
            target_changed: false,
        });
    }
    let mods = if let Some(mods) = existing_mods {
        mods
    } else {
        let instance = instance.clone();
        run_managed_blocking(PhysicalIoClass::Write, move || {
            instance.open_or_create_child("mods")
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("install_preflight"))?
        .map_err(ManagedMutationError::definite)?
    };
    let previous_state = load_state(&mods)
        .map_err(|error| classify_state_reconciliation_error("install_preflight", error))?;

    // Candidate intent and the exact transient leaf are durable before DNS
    // resolution or a provider request. Verified candidates remain internal
    // and cannot affect a live mod name before the target-effect boundary.
    let staged = stage_managed_graph(resolver, selection.pins, &mods)
        .await
        .map_err(ManagedMutationError::definite)?;
    let snapshot = match previous_state.as_ref() {
        Some(previous_state) => save_rollback_snapshot_async(&mods, previous_state)
            .await
            .map_err(|error| classify_state_reconciliation_error("install_snapshot", error))
            .map(|_| ()),
        None => save_absent_rollback_snapshot_async(&mods)
            .await
            .map_err(|error| classify_state_reconciliation_error("install_snapshot", error))
            .map(|_| ()),
    };
    if let Err(error) = snapshot {
        settle_pre_target_candidates(&mods, staged).await?;
        return Err(error.into());
    }

    if let Err(error) =
        state::require_cleanup_quarantine_empty(&mods).map_err(ManagedMutationError::definite)
    {
        settle_pre_target_candidates(&mods, staged).await?;
        return Err(error.into());
    }

    if let Err(error) = before_target_effect().await {
        settle_pre_target_candidates(&mods, staged).await?;
        return Err(ManagedInstallExecutionError::BeforeTargetEffect(error));
    }

    let mods_for_commit = mods.clone();
    let previous_for_commit = previous_state.clone();
    let state = state_from_plan(plan, installed_graph_from_plan(plan));
    let state_for_commit = state.clone();
    let commit = run_managed_blocking(PhysicalIoClass::Heavy, move || {
        commit_staged_graph(
            &mods_for_commit,
            previous_for_commit.as_ref(),
            &state_for_commit,
            staged,
        )
    })
    .await
    .unwrap_or_else(|_| {
        Err(InstallError::Io(std::io::Error::other(
            "managed graph commit task stopped before reporting completion",
        )))
    });
    if let Err(error) = commit {
        let snapshot = load_rollback_snapshot_async(&mods)
            .await
            .map_err(|rollback| classify_state_reconciliation_error("install_restore", rollback))?
            .ok_or_else(|| {
                ManagedMutationError::indeterminate(
                    "install_restore",
                    InstallError::NoRollbackSnapshot,
                )
            })?;
        restore_rollback_snapshot_classified_async(&mods, &snapshot)
            .await
            .map_err(|rollback| match rollback {
                RollbackRestoreError::Definite(rollback)
                | RollbackRestoreError::Indeterminate(rollback) => {
                    ManagedMutationError::indeterminate("install_restore", rollback)
                }
            })?;
        return Err(ManagedMutationError::definite(error).into());
    }
    Ok(ManagedInstallExecutionOutcome {
        state,
        target_changed: true,
    })
}

#[cfg(any(test, feature = "test-support"))]
async fn settle_pre_target_candidates(
    instance_mods: &ManagedStorageDirectory,
    staged: Vec<StagedManagedArtifact>,
) -> Result<(), ManagedMutationError> {
    let instance_mods = instance_mods.clone();
    run_managed_blocking(PhysicalIoClass::Heavy, move || {
        drop(staged);
        state::reconcile_managed_storage(&instance_mods)
    })
    .await
    .map_err(|_| ManagedMutationError::task_stopped("install_candidate_cleanup"))?
    .map_err(|error| ManagedMutationError::indeterminate("install_candidate_cleanup", error))
}

#[cfg(any(test, feature = "test-support"))]
fn commit_staged_graph<Stage: ManagedArtifactStage>(
    instance_mods: &ManagedStorageDirectory,
    previous_state: Option<&CompositionState>,
    state: &CompositionState,
    staged: Vec<Stage>,
) -> Result<(), InstallError> {
    state::require_cleanup_quarantine_empty(instance_mods)?;
    let desired_by_filename = state
        .installed_mods
        .iter()
        .map(|installed| Ok((commit_filename_key(&installed.filename)?, installed)))
        .collect::<Result<std::collections::BTreeMap<_, _>, InstallError>>()?;
    let mut staged_by_filename = std::collections::BTreeMap::new();
    for artifact in &staged {
        let installed = artifact.installed();
        let filename_key = commit_filename_key(&installed.filename)?;
        let Some(desired) = desired_by_filename.get(&filename_key) else {
            return Err(invalid_commit_graph());
        };
        if !same_artifact_metadata(installed, desired)
            || staged_by_filename.insert(filename_key, installed).is_some()
        {
            return Err(invalid_commit_graph());
        }
    }

    // Prove every unstaged desired artifact immediately before the first mutation.
    let mut retained_filenames = std::collections::BTreeSet::new();
    for desired in &state.installed_mods {
        let previous = previous_state
            .into_iter()
            .flat_map(|state| state.installed_mods.iter())
            .find(|previous| same_artifact_metadata(previous, desired));
        let retained = if previous.is_some() {
            state::managed_artifact_matches(instance_mods, desired)?
        } else {
            false
        };
        if retained {
            retained_filenames.insert(commit_filename_key(&desired.filename)?);
        } else if !staged_by_filename.contains_key(&commit_filename_key(&desired.filename)?) {
            return Err(invalid_commit_graph());
        }
    }

    for previous in previous_state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !retained_filenames.contains(&commit_filename_key(&previous.filename)?) {
            stage_managed_artifact_removal(instance_mods, previous)?;
        }
    }

    for artifact in staged {
        let installed = artifact.installed().clone();
        if retained_filenames.contains(&commit_filename_key(&installed.filename)?) {
            drop(artifact);
            continue;
        }
        artifact.publish_create_new(instance_mods)?;
    }

    state::reconcile_managed_candidate_intents(instance_mods)?;
    save_state(instance_mods, state)?;
    state::reconcile_managed_addition_obligations(instance_mods, Some(state))?;
    for previous in previous_state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !retained_filenames.contains(&commit_filename_key(&previous.filename)?) {
            settle_managed_artifact_removal(instance_mods, previous)?;
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn invalid_commit_graph() -> InstallError {
    InstallError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "staged managed graph is incomplete or does not match its sealed plan",
    ))
}

#[cfg(any(test, feature = "test-support"))]
fn commit_filename_key(filename: &str) -> Result<PortablePathKey, InstallError> {
    PortableFileName::new_exact(filename)
        .map(|filename| filename.key())
        .map_err(|_| invalid_commit_graph())
}

#[cfg(any(test, feature = "test-support"))]
struct ManagedStageSelection {
    exact_state: Option<CompositionState>,
    pins: Vec<ManagedArtifactPin>,
}

#[cfg(any(test, feature = "test-support"))]
fn managed_stage_selection(
    instance_mods: &ManagedStorageDirectory,
    plan: &ManagedCompositionInstallPlan,
) -> Result<ManagedStageSelection, InstallError> {
    let preflight = state::preflight_managed_inspection_reconciliation(instance_mods)?;
    if preflight.state_publication_required() || preflight.admitted_state_reconciliation_required()
    {
        return Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        });
    }
    let Some(state) = state::load_state_admitted(instance_mods)? else {
        return Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        });
    };
    plan::validate_state_graph(&state)
        .map_err(|error| InstallError::State(state::StateError::InvalidState(error.to_string())))?;
    let target_matches = state.graph_sha512 == plan.graph_digest()
        && state.composition_id == plan.composition_id()
        && state.game_version == plan.game_version()
        && state.loader == plan.loader()
        && state.family == plan.family()
        && state.tier == plan.tier()
        && state.installed_mods.len() == plan.pins().len();
    let desired = installed_graph_from_plan(plan);
    let mut pins = Vec::new();
    for (pin, desired) in plan.pins().iter().zip(&desired) {
        let reusable = state
            .installed_mods
            .iter()
            .find(|installed| installed.project_id == desired.project_id)
            .is_some_and(|installed| same_artifact_metadata(installed, desired))
            && state::managed_artifact_matches(instance_mods, desired)?;
        if !reusable {
            pins.push(pin.clone());
        }
    }
    if target_matches && pins.is_empty() {
        return Ok(ManagedStageSelection {
            exact_state: Some(state),
            pins,
        });
    }
    Ok(ManagedStageSelection {
        exact_state: None,
        pins,
    })
}

#[cfg(any(test, feature = "test-support"))]
fn same_artifact_metadata(left: &InstalledMod, right: &InstalledMod) -> bool {
    left.project_id == right.project_id
        && left.version_id == right.version_id
        && left.filename == right.filename
        && left.role == right.role
        && left.size == right.size
        && left.ownership_class == right.ownership_class
        && left.source == right.source
        && left.integrity == right.integrity
}

#[cfg(any(test, feature = "test-support"))]
fn classify_rollback_restore_error(error: RollbackRestoreError) -> ManagedMutationError {
    match error {
        RollbackRestoreError::Definite(error) => ManagedMutationError::definite(error),
        RollbackRestoreError::Indeterminate(error) => {
            ManagedMutationError::indeterminate("rollback", error)
        }
    }
}

fn classify_state_reconciliation_error(
    operation: &'static str,
    error: state::StateError,
) -> ManagedMutationError {
    if matches!(&error, state::StateError::RollbackCandidateUnresumable) {
        ManagedMutationError::definite(error)
    } else {
        ManagedMutationError::indeterminate(operation, error)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn classify_install_reconciliation_error(
    operation: &'static str,
    error: InstallError,
) -> ManagedMutationError {
    if matches!(
        &error,
        InstallError::State(state::StateError::RollbackCandidateUnresumable)
    ) {
        ManagedMutationError::definite(error)
    } else {
        ManagedMutationError::indeterminate(operation, error)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn remove_managed_transaction(instance_mods: &ManagedStorageDirectory) -> Result<(), InstallError> {
    let Some(state) = load_state(instance_mods)? else {
        return Ok(());
    };
    save_rollback_snapshot(instance_mods, &state)?;
    let result = (|| -> Result<(), InstallError> {
        for installed in &state.installed_mods {
            stage_managed_artifact_removal(instance_mods, installed)?;
        }
        remove_state(instance_mods)?;
        for installed in &state.installed_mods {
            settle_managed_artifact_removal(instance_mods, installed)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        load_state(instance_mods)?;
        rollback_managed_transaction(instance_mods)?;
        return Err(error);
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn rollback_managed_transaction(
    instance_mods: &ManagedStorageDirectory,
) -> Result<ManagedRollbackOutcome, InstallError> {
    let snapshot =
        load_rollback_snapshot(instance_mods)?.ok_or(InstallError::NoRollbackSnapshot)?;
    Ok(restore_rollback_snapshot(instance_mods, &snapshot)?)
}

#[cfg(test)]
mod witness_tests {
    use super::*;

    #[test]
    fn ownership_protects_enabled_and_disabled_aliases_even_when_bytes_changed() {
        let proof = ManagedArtifactWitnessProof {
            filename: PortableFileName::new_exact("Sodium.jar").unwrap().key(),
        };
        assert!(proof.protects_filename("sodium.jar"));
        assert!(proof.protects_filename("SODIUM.JAR.DISABLED"));
        assert!(!proof.protects_filename("other.jar"));
        assert!(!proof.protects_filename("../Sodium.jar"));
    }
}
