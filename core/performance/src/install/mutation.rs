use super::artifact::{
    ManagedArtifactStage, installed_graph_from_plan, stage_managed_graph, state_from_plan,
};
use super::manager::{ManagedCompositionAuthority, ManagedInstanceIdentity, PerformanceManager};
use super::model::InstallError;
use super::plan::{ManagedArtifactPin, ManagedCompositionInstallPlan};
use crate::state::{
    ManagedRollbackOutcome, RollbackRestoreError, RollbackSnapshotSummary, load_rollback_snapshot,
    load_rollback_snapshot_async, load_rollback_snapshot_by_id_async, load_state,
    prepare_managed_artifact_addition, publish_managed_artifact_addition, remove_state,
    restore_rollback_snapshot, restore_rollback_snapshot_classified_async,
    save_absent_rollback_snapshot_async, save_rollback_snapshot, save_rollback_snapshot_async,
    save_state, settle_managed_artifact_removal, stage_managed_artifact_removal,
};
use crate::storage::{ManagedInstanceEffectAuthority, ManagedStorageDirectory};
use crate::types::{CompositionPlan, CompositionState, InstalledMod, ResolutionRequest};
use axial_fs::{Directory, DirectoryListingState, EntryKind, LeafName};
use axial_minecraft::portable_path::{PortableFileName, PortablePathKey};

#[derive(Clone, Debug)]
pub struct ManagedCompositionInspection {
    pub state: Option<CompositionState>,
    pub health: crate::health::BundleHealth,
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
    rollback_ready: bool,
}

impl ManagedInstallExecutionOutcome {
    pub fn into_state(self) -> CompositionState {
        self.state
    }

    pub fn target_changed(&self) -> bool {
        self.target_changed
    }

    pub fn rollback_ready(&self) -> bool {
        self.rollback_ready
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedInstallExecutionError<BeforeTargetEffectError> {
    #[error("{source}")]
    Mutation {
        #[source]
        source: ManagedMutationError,
        rollback_ready: bool,
    },
    #[error("managed composition target-effect checkpoint was rejected")]
    BeforeTargetEffect {
        error: BeforeTargetEffectError,
        rollback_ready: bool,
    },
}

impl<BeforeTargetEffectError> ManagedInstallExecutionError<BeforeTargetEffectError> {
    pub fn from_mutation(source: ManagedMutationError, rollback_ready: bool) -> Self {
        Self::Mutation {
            source,
            rollback_ready,
        }
    }

    pub fn rollback_ready(&self) -> bool {
        match self {
            Self::Mutation { rollback_ready, .. }
            | Self::BeforeTargetEffect { rollback_ready, .. } => *rollback_ready,
        }
    }

    pub fn mutation_error(&self) -> Option<&ManagedMutationError> {
        match self {
            Self::Mutation { source, .. } => Some(source),
            Self::BeforeTargetEffect { .. } => None,
        }
    }
}

pub struct ManagedArtifactWitnessProof {
    filename: PortablePathKey,
    sha512: String,
}

impl ManagedArtifactWitnessProof {
    pub fn matches_observation(&self, filename: &str, sha512: &str) -> bool {
        PortableFileName::new_exact(filename)
            .is_ok_and(|filename| filename.key() == self.filename)
            && self.sha512.eq_ignore_ascii_case(sha512)
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

impl ManagedIndeterminate {
    pub fn operation(&self) -> &'static str {
        self.operation
    }
}

impl ManagedMutationError {
    pub(super) fn definite(error: impl Into<InstallError>) -> Self {
        Self::Definite(error.into())
    }

    pub(super) fn indeterminate(operation: &'static str, error: impl Into<InstallError>) -> Self {
        Self::Indeterminate(ManagedIndeterminate {
            operation,
            source: ManagedIndeterminateSource::Install(error.into()),
        })
    }

    pub fn owner_stopped(operation: &'static str) -> Self {
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

    fn task_stopped(operation: &'static str) -> Self {
        Self::owner_stopped(operation)
    }
}

impl ManagedCompositionAuthority {
    pub async fn bind_instance_effect_authority(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<ManagedInstanceEffectAuthority, ManagedMutationError> {
        let instance = self.open_instance_directory(identity).await?;
        let anchor_instance = instance.clone();
        let instance_anchor = tokio::task::spawn_blocking(move || anchor_instance.identity())
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
        let candidate = tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
            let state = crate::state::load_state_admitted(&mods)
                .map_err(ManagedMutationError::definite)?;
            let mut proofs = state
                .into_iter()
                .flat_map(|state| state.installed_mods)
                .map(|installed| ManagedArtifactWitnessProof {
                    filename: PortableFileName::new_exact(&installed.filename)
                        .expect("admitted performance state has portable filenames")
                        .key(),
                    sha512: installed.integrity.sha512,
                })
                .collect::<Vec<_>>();
            proofs.sort_by(|left, right| {
                (&left.filename, &left.sha512).cmp(&(&right.filename, &right.sha512))
            });
            proofs.dedup_by(|left, right| {
                left.filename == right.filename && left.sha512 == right.sha512
            });
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
        tokio::task::spawn_blocking(move || {
            settle_effects.settle()?;
            settle_effects.require_settled()
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("recover_effects"))?
        .map_err(|error| ManagedMutationError::indeterminate("recover_effects", error))?;
        let inspection = if let Some(mods) = open_mods_if_present(instance.clone(), "recover").await?
        {
            let recovery_mods = mods.clone();
            tokio::task::spawn_blocking(move || {
                crate::state::recover_managed_storage(&recovery_mods)
            })
            .await
            .map_err(|_| ManagedMutationError::task_stopped("recover"))?
            .map_err(|error| classify_state_reconciliation_error("recover", error))?;
            tokio::task::spawn_blocking(move || recovered_inspection(mods))
                .await
                .map_err(|_| ManagedMutationError::task_stopped("recover"))??
        } else {
            absent_inspection(None, None)
        };
        let final_effects = effects.clone();
        tokio::task::spawn_blocking(move || final_effects.require_settled())
            .await
            .map_err(|_| ManagedMutationError::task_stopped("recover_effects"))?
            .map_err(|error| ManagedMutationError::indeterminate("recover_effects", error))?;
        Ok(inspection)
    }

    pub async fn ensure_installed<
        BeforeTargetEffect,
        BeforeTargetEffectFuture,
        BeforeTargetEffectError,
    >(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
        plan: &ManagedCompositionInstallPlan,
        client: &reqwest::Client,
        before_target_effect: BeforeTargetEffect,
    ) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
    where
        BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture,
        BeforeTargetEffectFuture: std::future::Future<Output = Result<(), BeforeTargetEffectError>>,
    {
        let instance = self
            .validate_identity(identity, effects)
            .await
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        self.manager
            .ensure_installed(plan, client, &instance, before_target_effect)
            .await
    }

    pub async fn remove_managed(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
    ) -> Result<(), ManagedMutationError> {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) = open_mods_if_present(instance, "remove").await? else {
            return Ok(());
        };
        self.manager.remove_managed_async(mods).await
    }

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
        self.manager.rollback_managed_async(mods).await
    }

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
        self.manager
            .rollback_managed_snapshot_async(mods, snapshot_id)
            .await
    }

    pub async fn inspect<AdmitMutation, MutationPermit>(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
        plan: Option<&CompositionPlan>,
        admit_mutation: AdmitMutation,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError>
    where
        AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError> + Send + 'static,
        MutationPermit: Send + 'static,
    {
        let instance = self.validate_identity(identity, effects).await?;
        let Some(mods) = open_mods_if_present(instance.clone(), "inspect").await? else {
            return Ok(absent_inspection(plan, None));
        };
        let plan = plan.cloned();
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let (state, mutation_permit) = admitted_inspection_state(&mods, admit_mutation)?;
            let (health, warnings) = crate::health::derive_health(
                state.as_ref(),
                plan.as_ref(),
                None,
                Some(&mods),
            );
            let installed_mod_evidence = installed_mod_evidence(&mods, state.as_ref())
                .map_err(ManagedMutationError::definite)?;
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(&mods)
                .map_err(ManagedMutationError::definite)?;
            let inspection = ManagedCompositionInspection {
                state,
                health,
                warnings,
                installed_mod_evidence,
                rollback_snapshots,
            };
            drop(mutation_permit);
            Ok(inspection)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("inspect"))?
    }

    pub async fn resolve_and_inspect<AdmitMutation, MutationPermit>(
        &self,
        identity: &ManagedInstanceIdentity,
        effects: &ManagedInstanceEffectAuthority,
        mut request: ResolutionRequest,
        admit_mutation: AdmitMutation,
    ) -> Result<ManagedResolvedInspection, ManagedMutationError>
    where
        AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError> + Send + 'static,
        MutationPermit: Send + 'static,
    {
        let instance = self.validate_identity(identity, effects).await?;
        let mods = open_mods_if_present(instance.clone(), "inspect").await?;
        let manager = self.manager.clone();
        if mods.is_none() {
            request.installed_mods.clear();
            let expected_game_version = request.game_version.clone();
            let plan = manager.get_plan(request);
            return Ok(ManagedResolvedInspection {
                inspection: absent_inspection(Some(&plan), Some(&expected_game_version)),
                plan,
            });
        }
        let mods = mods.expect("managed mods capability was checked");
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let (state, mutation_permit) = admitted_inspection_state(&mods, admit_mutation)?;
            let installed_mod_evidence = installed_mod_evidence(&mods, state.as_ref())
                .map_err(ManagedMutationError::definite)?;
            request.installed_mods = installed_mod_evidence.clone();
            let expected_game_version = request.game_version.clone();
            let plan = manager.get_plan(request);
            let (health, warnings) = crate::health::derive_health(
                state.as_ref(),
                Some(&plan),
                Some(&expected_game_version),
                Some(&mods),
            );
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(&mods)
                .map_err(ManagedMutationError::definite)?;
            let inspection = ManagedResolvedInspection {
                inspection: ManagedCompositionInspection {
                    state,
                    health,
                    warnings,
                    installed_mod_evidence,
                    rollback_snapshots,
                },
                plan,
            };
            drop(mutation_permit);
            Ok(inspection)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("inspect"))?
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
        let instances_root = self.instances_root_directory().clone();
        let instance_id = identity.instance_id().to_string();
        tokio::task::spawn_blocking(move || {
            let instance_id = LeafName::new(instance_id).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "managed composition instance identity is not a direct leaf",
                )
            })?;
            instances_root.open_directory(&instance_id)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("identity_validation"))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
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
    tokio::task::spawn_blocking(move || instance.open_child("mods"))
        .await
        .map_err(|_| ManagedMutationError::task_stopped(operation))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
}

fn absent_inspection(
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
) -> ManagedCompositionInspection {
    let (health, warnings) =
        crate::health::derive_health(None, plan, expected_game_version, None);
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
    let state = crate::state::load_state_admitted(&instance_mods)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    crate::state::prove_managed_storage_recovered(&instance_mods, state.as_ref())
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    let (health, warnings) =
        crate::health::derive_health(state.as_ref(), None, None, Some(&instance_mods));
    let installed_mod_evidence = installed_mod_evidence(&instance_mods, state.as_ref())
        .map_err(ManagedMutationError::definite)?;
    let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(&instance_mods)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    Ok(ManagedCompositionInspection {
        state,
        health,
        warnings,
        installed_mod_evidence,
        rollback_snapshots,
    })
}

fn admitted_inspection_state<AdmitMutation, MutationPermit>(
    instance_mods: &ManagedStorageDirectory,
    admit_mutation: AdmitMutation,
) -> Result<(Option<CompositionState>, Option<MutationPermit>), ManagedMutationError>
where
    AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError>,
{
    let mut admit_mutation = Some(admit_mutation);
    let mut mutation_permit = None;
    if crate::state::managed_effect_reconciliation_required(instance_mods) {
        mutation_permit = Some(admit_inspection_mutation(&mut admit_mutation)?);
        instance_mods
            .settle_pending_effects()
            .map_err(InstallError::Io)
            .map_err(|error| {
                ManagedMutationError::indeterminate("inspect_effect_reconcile", error)
            })?;
    }
    let preflight = crate::state::preflight_managed_inspection_reconciliation(instance_mods)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    if preflight.state_publication_required() {
        if mutation_permit.is_none() {
            mutation_permit = Some(admit_inspection_mutation(&mut admit_mutation)?);
        }
    }
    crate::state::reconcile_managed_inspection_publication(instance_mods, preflight)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    let state = crate::state::load_state_admitted(instance_mods)
        .map_err(ManagedMutationError::definite)?;
    if mutation_permit.is_none() && preflight.admitted_state_reconciliation_required() {
        mutation_permit = Some(admit_inspection_mutation(&mut admit_mutation)?);
    }
    crate::state::reconcile_managed_inspection_obligations(
        instance_mods,
        preflight,
        state.as_ref(),
    )
    .map_err(|error| classify_state_reconciliation_error("inspect_reconcile", error))?;
    Ok((state, mutation_permit))
}

fn admit_inspection_mutation<AdmitMutation, MutationPermit>(
    admit_mutation: &mut Option<AdmitMutation>,
) -> Result<MutationPermit, ManagedMutationError>
where
    AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError>,
{
    admit_mutation
        .take()
        .expect("inspection mutation callback is available before the first effect")()
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
        .entries(crate::state::RECOVERY_ENTRY_LIMIT + 1)
        .map_err(InstallError::Io)?;
    if listing.state() != DirectoryListingState::Complete
        || listing.entries().len() > crate::state::RECOVERY_ENTRY_LIMIT
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

impl PerformanceManager {
    pub(super) async fn ensure_installed<
        BeforeTargetEffect,
        BeforeTargetEffectFuture,
        BeforeTargetEffectError,
    >(
        &self,
        plan: &ManagedCompositionInstallPlan,
        client: &reqwest::Client,
        instance: &ManagedStorageDirectory,
        before_target_effect: BeforeTargetEffect,
    ) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
    where
        BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture,
        BeforeTargetEffectFuture: std::future::Future<Output = Result<(), BeforeTargetEffectError>>,
    {
        let existing_mods = open_mods_if_present(instance.clone(), "install_preflight")
            .await
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        let selection = match existing_mods.as_ref() {
            Some(mods) => managed_stage_selection(mods, plan),
            None => Ok(ManagedStageSelection {
                exact_state: None,
                pins: plan.pins().to_vec(),
            }),
        }
        .map_err(ManagedMutationError::definite)
        .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        if let Some(state) = selection.exact_state {
            return Ok(ManagedInstallExecutionOutcome {
                state,
                target_changed: false,
                rollback_ready: false,
            });
        }
        // Provider and network work is completed while ownership remains entirely
        // in anonymous staging handles. No rollback or managed state effect exists yet.
        let staged = stage_managed_graph(client, selection.pins, instance)
            .await
            .map_err(ManagedMutationError::definite)
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        let mods = if let Some(mods) = existing_mods {
            mods
        } else {
            let instance = instance.clone();
            tokio::task::spawn_blocking(move || instance.open_or_create_child("mods"))
                .await
                .map_err(|_| ManagedMutationError::task_stopped("install_preflight"))
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?
                .map_err(ManagedMutationError::definite)
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?
        };
        let previous_state = load_state(&mods)
            .map_err(|error| classify_state_reconciliation_error("install_preflight", error))
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        match previous_state.as_ref() {
            Some(previous_state) => save_rollback_snapshot_async(&mods, previous_state)
                .await
                .map_err(|error| classify_state_reconciliation_error("install_snapshot", error))
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?,
            None => save_absent_rollback_snapshot_async(&mods)
                .await
                .map_err(|error| classify_state_reconciliation_error("install_snapshot", error))
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?,
        };

        crate::state::require_cleanup_quarantine_empty(&mods)
            .map_err(ManagedMutationError::definite)
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?;

        before_target_effect().await.map_err(|error| {
            ManagedInstallExecutionError::BeforeTargetEffect {
                error,
                rollback_ready: true,
            }
        })?;

        let mods_for_commit = mods.clone();
        let previous_for_commit = previous_state.clone();
        let state = state_from_plan(plan, installed_graph_from_plan(plan));
        let state_for_commit = state.clone();
        let commit = tokio::task::spawn_blocking(move || {
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
                .map_err(|rollback| {
                    classify_state_reconciliation_error("install_restore", rollback)
                })
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?
                .ok_or_else(|| {
                    ManagedMutationError::indeterminate(
                        "install_restore",
                        InstallError::NoRollbackSnapshot,
                    )
                })
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?;
            restore_rollback_snapshot_classified_async(&mods, &snapshot)
                .await
                .map_err(|rollback| match rollback {
                    RollbackRestoreError::Definite(rollback)
                    | RollbackRestoreError::Indeterminate(rollback) => {
                        ManagedMutationError::indeterminate("install_restore", rollback)
                    }
                })
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?;
            return Err(ManagedInstallExecutionError::from_mutation(
                ManagedMutationError::definite(error),
                true,
            ));
        }
        Ok(ManagedInstallExecutionOutcome {
            state,
            target_changed: true,
            rollback_ready: true,
        })
    }

    pub(super) async fn remove_managed_async(
        &self,
        instance_mods: ManagedStorageDirectory,
    ) -> Result<(), ManagedMutationError> {
        tokio::task::spawn_blocking(move || remove_managed_transaction(&instance_mods))
            .await
            .map_err(|_| ManagedMutationError::task_stopped("remove"))?
            .map_err(|error| classify_install_reconciliation_error("remove", error))
    }

    pub(super) async fn rollback_managed_async(
        &self,
        instance_mods: ManagedStorageDirectory,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let snapshot = load_rollback_snapshot_async(&instance_mods)
            .await
            .map_err(|error| classify_state_reconciliation_error("rollback_preflight", error))?
            .ok_or_else(|| ManagedMutationError::definite(InstallError::NoRollbackSnapshot))?;
        restore_rollback_snapshot_classified_async(&instance_mods, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }

    pub(super) async fn rollback_managed_snapshot_async(
        &self,
        instance_mods: ManagedStorageDirectory,
        snapshot_id: &str,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        crate::state::validate_rollback_snapshot_id(snapshot_id)
            .map_err(ManagedMutationError::definite)?;
        let snapshot = load_rollback_snapshot_by_id_async(&instance_mods, snapshot_id)
            .await
            .map_err(|error| classify_state_reconciliation_error("rollback_preflight", error))?
            .ok_or_else(|| {
                ManagedMutationError::definite(InstallError::RollbackSnapshotNotFound)
            })?;
        restore_rollback_snapshot_classified_async(&instance_mods, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }
}

pub(super) fn commit_staged_graph<Stage: ManagedArtifactStage>(
    instance_mods: &ManagedStorageDirectory,
    previous_state: Option<&CompositionState>,
    state: &CompositionState,
    staged: Vec<Stage>,
) -> Result<(), InstallError> {
    crate::state::require_cleanup_quarantine_empty(instance_mods)?;
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
            || staged_by_filename
                .insert(filename_key, installed)
                .is_some()
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
            crate::state::managed_artifact_matches(instance_mods, desired)?
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
        let obligation = prepare_managed_artifact_addition(instance_mods, &installed)?;
        artifact.publish_create_new(obligation.parent(), obligation.filename())?;
        publish_managed_artifact_addition(instance_mods, &installed, &obligation)?;
    }

    save_state(instance_mods, state)?;
    crate::state::reconcile_managed_addition_obligations(instance_mods, Some(state))?;
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

fn invalid_commit_graph() -> InstallError {
    InstallError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "staged managed graph is incomplete or does not match its sealed plan",
    ))
}

fn commit_filename_key(filename: &str) -> Result<PortablePathKey, InstallError> {
    PortableFileName::new_exact(filename)
        .map(|filename| filename.key())
        .map_err(|_| invalid_commit_graph())
}

pub(super) struct ManagedStageSelection {
    pub(super) exact_state: Option<CompositionState>,
    pub(super) pins: Vec<ManagedArtifactPin>,
}

pub(super) fn managed_stage_selection(
    instance_mods: &ManagedStorageDirectory,
    plan: &ManagedCompositionInstallPlan,
) -> Result<ManagedStageSelection, InstallError> {
    let preflight = crate::state::preflight_managed_inspection_reconciliation(instance_mods)?;
    if preflight.state_publication_required() || preflight.admitted_state_reconciliation_required()
    {
        return Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        });
    }
    let Some(state) = crate::state::load_state_admitted(instance_mods)? else {
        return Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        });
    };
    crate::install::plan::validate_state_graph(&state).map_err(|error| {
        InstallError::State(crate::state::StateError::InvalidState(error.to_string()))
    })?;
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
            && crate::state::managed_artifact_matches(instance_mods, desired)?;
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
    error: crate::state::StateError,
) -> ManagedMutationError {
    if matches!(
        &error,
        crate::state::StateError::RollbackCandidateUnresumable
    ) {
        ManagedMutationError::definite(error)
    } else {
        ManagedMutationError::indeterminate(operation, error)
    }
}

fn classify_install_reconciliation_error(
    operation: &'static str,
    error: InstallError,
) -> ManagedMutationError {
    if matches!(
        &error,
        InstallError::State(crate::state::StateError::RollbackCandidateUnresumable)
    ) {
        ManagedMutationError::definite(error)
    } else {
        ManagedMutationError::indeterminate(operation, error)
    }
}

pub(super) fn remove_managed_transaction(
    instance_mods: &ManagedStorageDirectory,
) -> Result<(), InstallError> {
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

fn rollback_managed_transaction(
    instance_mods: &ManagedStorageDirectory,
) -> Result<ManagedRollbackOutcome, InstallError> {
    let snapshot =
        load_rollback_snapshot(instance_mods)?.ok_or(InstallError::NoRollbackSnapshot)?;
    Ok(restore_rollback_snapshot(instance_mods, &snapshot)?)
}
