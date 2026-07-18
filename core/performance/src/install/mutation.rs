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
use crate::types::{CompositionPlan, CompositionState, InstalledMod, ResolutionRequest};
use axial_minecraft::managed_path::AnchoredDirectory;
use std::fs;
use std::path::Path;

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
    filename: String,
    sha512: String,
}

impl ManagedArtifactWitnessProof {
    pub fn matches_observation(&self, filename: &str, sha512: &str) -> bool {
        let filename_matches = if cfg!(windows) {
            self.filename.eq_ignore_ascii_case(filename)
        } else {
            self.filename == filename
        };
        filename_matches && self.sha512.eq_ignore_ascii_case(sha512)
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

    fn task_stopped(operation: &'static str) -> Self {
        Self::Indeterminate(ManagedIndeterminate {
            operation,
            source: ManagedIndeterminateSource::TaskStopped,
        })
    }
}

impl ManagedCompositionAuthority {
    pub async fn composition_managed_witness_proofs(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<Vec<ManagedArtifactWitnessProof>, ManagedMutationError> {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) =
            open_mods_if_present(instance, "composition_managed_witness_proofs").await?
        else {
            return Ok(Vec::new());
        };
        tokio::task::spawn_blocking(move || {
            let mods_dir = mods.path();
            let state = crate::state::load_state_admitted(mods_dir)
                .map_err(ManagedMutationError::definite)?;
            let mut proofs = state
                .into_iter()
                .flat_map(|state| state.installed_mods)
                .map(|installed| ManagedArtifactWitnessProof {
                    filename: installed.filename,
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
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) = open_mods_if_present(instance.clone(), "recover").await? else {
            return Ok(absent_inspection(None, None, instance.path()));
        };
        let recovery_mods = mods.clone();
        tokio::task::spawn_blocking(move || {
            crate::state::recover_managed_storage(recovery_mods.path())
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("recover"))?
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
        tokio::task::spawn_blocking(move || recovered_inspection(mods.path()))
            .await
            .map_err(|_| ManagedMutationError::task_stopped("recover"))?
    }

    pub async fn ensure_installed<
        BeforeTargetEffect,
        BeforeTargetEffectFuture,
        BeforeTargetEffectError,
    >(
        &self,
        identity: &ManagedInstanceIdentity,
        plan: &ManagedCompositionInstallPlan,
        client: &reqwest::Client,
        before_target_effect: BeforeTargetEffect,
    ) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
    where
        BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture,
        BeforeTargetEffectFuture: std::future::Future<Output = Result<(), BeforeTargetEffectError>>,
    {
        let instance = self
            .validate_identity(identity)
            .await
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        self.manager
            .ensure_installed(plan, client, &instance, before_target_effect)
            .await
    }

    pub async fn remove_managed(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<(), ManagedMutationError> {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) = open_mods_if_present(instance, "remove").await? else {
            return Ok(());
        };
        self.manager.remove_managed_async(mods.path()).await
    }

    pub async fn rollback_managed(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) = open_mods_if_present(instance, "rollback_preflight").await? else {
            return Err(ManagedMutationError::definite(
                InstallError::NoRollbackSnapshot,
            ));
        };
        self.manager.rollback_managed_async(mods.path()).await
    }

    pub async fn rollback_managed_snapshot(
        &self,
        identity: &ManagedInstanceIdentity,
        snapshot_id: &str,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) = open_mods_if_present(instance, "rollback_preflight").await? else {
            return Err(ManagedMutationError::definite(
                InstallError::RollbackSnapshotNotFound,
            ));
        };
        self.manager
            .rollback_managed_snapshot_async(mods.path(), snapshot_id)
            .await
    }

    pub async fn inspect<AdmitMutation, MutationPermit>(
        &self,
        identity: &ManagedInstanceIdentity,
        plan: Option<&CompositionPlan>,
        admit_mutation: AdmitMutation,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError>
    where
        AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError> + Send + 'static,
        MutationPermit: Send + 'static,
    {
        let instance = self.validate_identity(identity).await?;
        let Some(mods) = open_mods_if_present(instance.clone(), "inspect").await? else {
            return Ok(absent_inspection(plan, None, instance.path()));
        };
        let plan = plan.cloned();
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let mods_dir = mods.path();
            let (state, mutation_permit) = admitted_inspection_state(mods_dir, admit_mutation)?;
            let (health, warnings) =
                crate::health::derive_health(state.as_ref(), plan.as_ref(), None, mods_dir);
            let installed_mod_evidence = installed_mod_evidence(mods_dir, state.as_ref());
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(mods_dir)
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
        mut request: ResolutionRequest,
        admit_mutation: AdmitMutation,
    ) -> Result<ManagedResolvedInspection, ManagedMutationError>
    where
        AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError> + Send + 'static,
        MutationPermit: Send + 'static,
    {
        let instance = self.validate_identity(identity).await?;
        let mods = open_mods_if_present(instance.clone(), "inspect").await?;
        let manager = self.manager.clone();
        if mods.is_none() {
            request.installed_mods.clear();
            let expected_game_version = request.game_version.clone();
            let plan = manager.get_plan(request);
            return Ok(ManagedResolvedInspection {
                inspection: absent_inspection(
                    Some(&plan),
                    Some(&expected_game_version),
                    instance.path(),
                ),
                plan,
            });
        }
        let mods = mods.expect("managed mods capability was checked");
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let mods_dir = mods.path();
            let (state, mutation_permit) = admitted_inspection_state(mods_dir, admit_mutation)?;
            let installed_mod_evidence = installed_mod_evidence(mods_dir, state.as_ref());
            request.installed_mods = installed_mod_evidence.clone();
            let expected_game_version = request.game_version.clone();
            let plan = manager.get_plan(request);
            let (health, warnings) = crate::health::derive_health(
                state.as_ref(),
                Some(&plan),
                Some(&expected_game_version),
                mods_dir,
            );
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(mods_dir)
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
    ) -> Result<AnchoredDirectory, ManagedMutationError> {
        let expected = self
            .instances_root()
            .join(identity.instance_id())
            .join("mods");
        if identity.mods_dir() != expected {
            return Err(ManagedMutationError::definite(InstallError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "managed composition identity path drifted from its authority root",
                ),
            )));
        }
        let instances_root = self.instances_root_anchor().clone();
        let instance_id = identity.instance_id().to_string();
        tokio::task::spawn_blocking(move || {
            instances_root.open_child(&instance_id)?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "managed composition instance directory does not exist",
                )
            })
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("identity_validation"))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
    }
}

async fn open_mods_if_present(
    instance: AnchoredDirectory,
    operation: &'static str,
) -> Result<Option<AnchoredDirectory>, ManagedMutationError> {
    tokio::task::spawn_blocking(move || instance.open_child("mods"))
        .await
        .map_err(|_| ManagedMutationError::task_stopped(operation))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
}

fn absent_inspection(
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
    instance_dir: &Path,
) -> ManagedCompositionInspection {
    let mods_dir = instance_dir.join("mods");
    let (health, warnings) =
        crate::health::derive_health(None, plan, expected_game_version, &mods_dir);
    ManagedCompositionInspection {
        state: None,
        health,
        warnings,
        installed_mod_evidence: Vec::new(),
        rollback_snapshots: Vec::new(),
    }
}

fn recovered_inspection(
    instance_mods_dir: &Path,
) -> Result<ManagedCompositionInspection, ManagedMutationError> {
    let state = crate::state::load_state_admitted(instance_mods_dir)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    crate::state::prove_managed_storage_recovered(instance_mods_dir, state.as_ref())
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    let (health, warnings) =
        crate::health::derive_health(state.as_ref(), None, None, instance_mods_dir);
    let installed_mod_evidence = installed_mod_evidence(instance_mods_dir, state.as_ref());
    let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(instance_mods_dir)
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
    instance_mods_dir: &Path,
    admit_mutation: AdmitMutation,
) -> Result<(Option<CompositionState>, Option<MutationPermit>), ManagedMutationError>
where
    AdmitMutation: FnOnce() -> Result<MutationPermit, ManagedMutationError>,
{
    let preflight = crate::state::preflight_managed_inspection_reconciliation(instance_mods_dir)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    let mut admit_mutation = Some(admit_mutation);
    let mut mutation_permit = None;
    if preflight.state_publication_required() {
        mutation_permit = Some(admit_inspection_mutation(&mut admit_mutation)?);
    }
    crate::state::reconcile_managed_inspection_publication(instance_mods_dir, preflight)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    let state = crate::state::load_state_admitted(instance_mods_dir)
        .map_err(ManagedMutationError::definite)?;
    if mutation_permit.is_none() && preflight.admitted_state_reconciliation_required() {
        mutation_permit = Some(admit_inspection_mutation(&mut admit_mutation)?);
    }
    crate::state::reconcile_managed_inspection_obligations(
        instance_mods_dir,
        preflight,
        state.as_ref(),
    )
    .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
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

fn installed_mod_evidence(mods_dir: &Path, state: Option<&CompositionState>) -> Vec<String> {
    let mut evidence = std::collections::BTreeSet::new();
    for installed in state.into_iter().flat_map(|state| &state.installed_mods) {
        add_mod_evidence(&mut evidence, &installed.project_id);
        add_mod_evidence(&mut evidence, &installed.filename);
    }
    if let Ok(entries) = fs::read_dir(mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case("jar"))
            {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                add_mod_evidence(&mut evidence, stem);
            }
        }
    }
    evidence.into_iter().collect()
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
        instance: &AnchoredDirectory,
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
            Some(mods) => managed_stage_selection(mods.path(), plan),
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
        let instance_mods_dir = mods.path();
        let previous_state = load_state(instance_mods_dir)
            .map_err(|error| ManagedMutationError::indeterminate("install_preflight", error))
            .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?;
        match previous_state.as_ref() {
            Some(previous_state) => save_rollback_snapshot_async(instance_mods_dir, previous_state)
                .await
                .map_err(|error| ManagedMutationError::indeterminate("install_snapshot", error))
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?,
            None => save_absent_rollback_snapshot_async(instance_mods_dir)
                .await
                .map_err(|error| ManagedMutationError::indeterminate("install_snapshot", error))
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, false))?,
        };

        crate::state::require_cleanup_quarantine_empty(instance_mods_dir)
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
            let snapshot = load_rollback_snapshot_async(instance_mods_dir)
                .await
                .map_err(|rollback| {
                    ManagedMutationError::indeterminate("install_restore", rollback)
                })
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?
                .ok_or_else(|| {
                    ManagedMutationError::indeterminate(
                        "install_restore",
                        InstallError::NoRollbackSnapshot,
                    )
                })
                .map_err(|error| ManagedInstallExecutionError::from_mutation(error, true))?;
            restore_rollback_snapshot_classified_async(instance_mods_dir, &snapshot)
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
        instance_mods_dir: &Path,
    ) -> Result<(), ManagedMutationError> {
        let instance_mods_dir = instance_mods_dir.to_path_buf();
        tokio::task::spawn_blocking(move || remove_managed_transaction(&instance_mods_dir))
            .await
            .map_err(|_| ManagedMutationError::task_stopped("remove"))?
            .map_err(|error| ManagedMutationError::indeterminate("remove", error))
    }

    pub(super) async fn rollback_managed_async(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let snapshot = load_rollback_snapshot_async(instance_mods_dir)
            .await
            .map_err(|error| ManagedMutationError::indeterminate("rollback_preflight", error))?
            .ok_or_else(|| ManagedMutationError::definite(InstallError::NoRollbackSnapshot))?;
        restore_rollback_snapshot_classified_async(instance_mods_dir, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }

    pub(super) async fn rollback_managed_snapshot_async(
        &self,
        instance_mods_dir: &Path,
        snapshot_id: &str,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        crate::state::validate_rollback_snapshot_id(snapshot_id)
            .map_err(ManagedMutationError::definite)?;
        let snapshot = load_rollback_snapshot_by_id_async(instance_mods_dir, snapshot_id)
            .await
            .map_err(|error| ManagedMutationError::indeterminate("rollback_preflight", error))?
            .ok_or_else(|| {
                ManagedMutationError::definite(InstallError::RollbackSnapshotNotFound)
            })?;
        restore_rollback_snapshot_classified_async(instance_mods_dir, &snapshot)
            .await
            .map_err(classify_rollback_restore_error)
    }
}

pub(super) fn commit_staged_graph<Stage: ManagedArtifactStage>(
    instance_mods: &AnchoredDirectory,
    previous_state: Option<&CompositionState>,
    state: &CompositionState,
    staged: Vec<Stage>,
) -> Result<(), InstallError> {
    let instance_mods_dir = instance_mods.path();
    crate::state::require_cleanup_quarantine_empty(instance_mods_dir)?;
    let desired_by_filename = state
        .installed_mods
        .iter()
        .map(|installed| (installed.filename.as_str(), installed))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut staged_by_filename = std::collections::BTreeMap::new();
    for artifact in &staged {
        let installed = artifact.installed();
        let Some(desired) = desired_by_filename.get(installed.filename.as_str()) else {
            return Err(invalid_commit_graph());
        };
        if !same_artifact_metadata(installed, desired)
            || staged_by_filename
                .insert(installed.filename.as_str(), installed)
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
            crate::state::managed_artifact_matches(instance_mods_dir, desired)?
        } else {
            false
        };
        if retained {
            retained_filenames.insert(desired.filename.as_str());
        } else if !staged_by_filename.contains_key(desired.filename.as_str()) {
            return Err(invalid_commit_graph());
        }
    }

    for previous in previous_state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !retained_filenames.contains(previous.filename.as_str()) {
            stage_managed_artifact_removal(instance_mods_dir, previous)?;
        }
    }

    for artifact in staged {
        let installed = artifact.installed().clone();
        if retained_filenames.contains(installed.filename.as_str()) {
            drop(artifact);
            continue;
        }
        let obligation = prepare_managed_artifact_addition(instance_mods, &installed)?;
        artifact.publish_create_new(obligation.parent(), obligation.filename())?;
        publish_managed_artifact_addition(instance_mods_dir, &installed, &obligation)?;
    }

    save_state(instance_mods_dir, state)?;
    crate::state::reconcile_managed_addition_obligations(instance_mods_dir, Some(state))?;
    for previous in previous_state
        .into_iter()
        .flat_map(|state| state.installed_mods.iter())
    {
        if !retained_filenames.contains(previous.filename.as_str()) {
            settle_managed_artifact_removal(instance_mods_dir, previous)?;
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

pub(super) struct ManagedStageSelection {
    pub(super) exact_state: Option<CompositionState>,
    pub(super) pins: Vec<ManagedArtifactPin>,
}

pub(super) fn managed_stage_selection(
    instance_mods_dir: &Path,
    plan: &ManagedCompositionInstallPlan,
) -> Result<ManagedStageSelection, InstallError> {
    let preflight = crate::state::preflight_managed_inspection_reconciliation(instance_mods_dir)?;
    if preflight.state_publication_required() || preflight.admitted_state_reconciliation_required()
    {
        return Ok(ManagedStageSelection {
            exact_state: None,
            pins: plan.pins().to_vec(),
        });
    }
    let Some(state) = crate::state::load_state_admitted(instance_mods_dir)? else {
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
            && crate::state::managed_artifact_matches(instance_mods_dir, desired)?;
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

pub(super) fn remove_managed_transaction(instance_mods_dir: &Path) -> Result<(), InstallError> {
    let Some(state) = load_state(instance_mods_dir)? else {
        return Ok(());
    };
    save_rollback_snapshot(instance_mods_dir, &state)?;
    let result = (|| -> Result<(), InstallError> {
        for installed in &state.installed_mods {
            stage_managed_artifact_removal(instance_mods_dir, installed)?;
        }
        remove_state(instance_mods_dir)?;
        for installed in &state.installed_mods {
            settle_managed_artifact_removal(instance_mods_dir, installed)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        load_state(instance_mods_dir)?;
        rollback_managed_transaction(instance_mods_dir)?;
        return Err(error);
    }
    Ok(())
}

fn rollback_managed_transaction(
    instance_mods_dir: &Path,
) -> Result<ManagedRollbackOutcome, InstallError> {
    let snapshot =
        load_rollback_snapshot(instance_mods_dir)?.ok_or(InstallError::NoRollbackSnapshot)?;
    Ok(restore_rollback_snapshot(instance_mods_dir, &snapshot)?)
}
