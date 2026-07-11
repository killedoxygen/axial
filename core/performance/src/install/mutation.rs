use super::fallback::{empty_state, severe_install_failure};
use super::manager::{ManagedCompositionAuthority, ManagedInstanceIdentity, PerformanceManager};
use super::model::InstallError;
use super::promotion::{reconcile_managed_replace_backups, settle_managed_replace_backup};
use crate::state::{
    RollbackRestoreError, RollbackSnapshotSummary, load_rollback_snapshot,
    load_rollback_snapshot_async, load_rollback_snapshot_by_id_async, load_state,
    remove_managed_artifact, remove_state, restore_rollback_snapshot,
    restore_rollback_snapshot_async, restore_rollback_snapshot_classified_async,
    save_rollback_snapshot, save_rollback_snapshot_async, save_state,
    settle_managed_artifact_removal, stage_managed_artifact_removal,
};
use crate::types::{
    CompositionPlan, CompositionState, InstalledMod, PerformanceMode, ResolutionRequest,
};
use std::fs;
use std::path::Path;
use tracing::warn;

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
    pub async fn recover_and_inspect(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        self.validate_identity(identity).await?;
        let mods_dir = identity.mods_dir().to_path_buf();
        let recovery_dir = mods_dir.clone();
        let state = tokio::task::spawn_blocking(move || {
            crate::state::recover_managed_storage(&recovery_dir)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("recover"))?
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
        super::artifact::reconcile_managed_artifact_obligations(&mods_dir, state.as_ref())
            .await
            .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
        tokio::task::spawn_blocking(move || recovered_inspection(&mods_dir))
            .await
            .map_err(|_| ManagedMutationError::task_stopped("recover"))?
    }

    pub async fn ensure_installed(
        &self,
        identity: &ManagedInstanceIdentity,
        plan: &CompositionPlan,
        game_version: &str,
    ) -> Result<CompositionState, ManagedMutationError> {
        self.validate_identity(identity).await?;
        self.manager
            .ensure_installed(plan, game_version, identity.mods_dir())
            .await
    }

    pub async fn remove_managed(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<(), ManagedMutationError> {
        self.validate_identity(identity).await?;
        self.manager.remove_managed_async(identity.mods_dir()).await
    }

    pub async fn rollback_managed(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<CompositionState, ManagedMutationError> {
        self.validate_identity(identity).await?;
        self.manager
            .rollback_managed_async(identity.mods_dir())
            .await
    }

    pub async fn rollback_managed_snapshot(
        &self,
        identity: &ManagedInstanceIdentity,
        snapshot_id: &str,
    ) -> Result<CompositionState, ManagedMutationError> {
        self.validate_identity(identity).await?;
        self.manager
            .rollback_managed_snapshot_async(identity.mods_dir(), snapshot_id)
            .await
    }

    pub async fn inspect(
        &self,
        identity: &ManagedInstanceIdentity,
        plan: Option<&CompositionPlan>,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        self.validate_identity(identity).await?;
        let mods_dir = identity.mods_dir().to_path_buf();
        let plan = plan.cloned();
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let state = admitted_inspection_state(&mods_dir)?;
            let (health, warnings) =
                crate::health::derive_health(state.as_ref(), plan.as_ref(), &mods_dir);
            let installed_mod_evidence = installed_mod_evidence(&mods_dir, state.as_ref());
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(&mods_dir)
                .map_err(ManagedMutationError::definite)?;
            Ok(ManagedCompositionInspection {
                state,
                health,
                warnings,
                installed_mod_evidence,
                rollback_snapshots,
            })
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("inspect"))?
    }

    pub async fn resolve_and_inspect(
        &self,
        identity: &ManagedInstanceIdentity,
        mut request: ResolutionRequest,
    ) -> Result<ManagedResolvedInspection, ManagedMutationError> {
        self.validate_identity(identity).await?;
        let mods_dir = identity.mods_dir().to_path_buf();
        let manager = self.manager.clone();
        tokio::task::spawn_blocking(move || -> Result<_, ManagedMutationError> {
            let state = admitted_inspection_state(&mods_dir)?;
            let installed_mod_evidence = installed_mod_evidence(&mods_dir, state.as_ref());
            request.installed_mods = installed_mod_evidence.clone();
            let plan = manager.get_plan(request);
            let (health, warnings) =
                crate::health::derive_health(state.as_ref(), Some(&plan), &mods_dir);
            let rollback_snapshots = crate::state::list_rollback_snapshots_admitted(&mods_dir)
                .map_err(ManagedMutationError::definite)?;
            Ok(ManagedResolvedInspection {
                inspection: ManagedCompositionInspection {
                    state,
                    health,
                    warnings,
                    installed_mod_evidence,
                    rollback_snapshots,
                },
                plan,
            })
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("inspect"))?
    }

    async fn validate_identity(
        &self,
        identity: &ManagedInstanceIdentity,
    ) -> Result<(), ManagedMutationError> {
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
        let instance_dir = expected
            .parent()
            .expect("managed mods path always has an instance parent")
            .to_path_buf();
        tokio::task::spawn_blocking(move || {
            validate_real_directory_if_present(&instance_dir)?;
            validate_real_directory_if_present(&expected)
        })
        .await
        .map_err(|_| ManagedMutationError::task_stopped("identity_validation"))?
        .map_err(|error| ManagedMutationError::definite(InstallError::Io(error)))
    }
}

fn recovered_inspection(
    instance_mods_dir: &Path,
) -> Result<ManagedCompositionInspection, ManagedMutationError> {
    let state = crate::state::load_state_admitted(instance_mods_dir)
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    crate::state::prove_managed_storage_recovered(instance_mods_dir, state.as_ref())
        .map_err(|error| ManagedMutationError::indeterminate("recover", error))?;
    let (health, warnings) = crate::health::derive_health(state.as_ref(), None, instance_mods_dir);
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

fn admitted_inspection_state(
    instance_mods_dir: &Path,
) -> Result<Option<CompositionState>, ManagedMutationError> {
    crate::state::reconcile_state_publication(instance_mods_dir)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    let state = crate::state::load_state_admitted(instance_mods_dir)
        .map_err(ManagedMutationError::definite)?;
    crate::state::reconcile_managed_removal_obligations(instance_mods_dir, state.as_ref())
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    crate::state::reconcile_rollback_metadata(instance_mods_dir)
        .map_err(|error| ManagedMutationError::indeterminate("inspect_reconcile", error))?;
    Ok(state)
}

fn validate_real_directory_if_present(path: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed composition identity contains a non-directory or symbolic link",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
    pub(super) async fn ensure_installed(
        &self,
        plan: &CompositionPlan,
        game_version: &str,
        instance_mods_dir: &Path,
    ) -> Result<CompositionState, ManagedMutationError> {
        if !matches!(plan.mode, PerformanceMode::Managed) {
            self.remove_managed_async(instance_mods_dir).await?;
            return Ok(empty_state(plan));
        }

        fs::create_dir_all(instance_mods_dir).map_err(ManagedMutationError::definite)?;
        let previous_state = load_state(instance_mods_dir)
            .map_err(|error| ManagedMutationError::indeterminate("install_preflight", error))?;
        if let Some(previous_state) = previous_state.as_ref() {
            save_rollback_snapshot_async(instance_mods_dir, previous_state)
                .await
                .map_err(|error| ManagedMutationError::indeterminate("install_snapshot", error))?;
        }

        let attempt_plans = self.install_attempt_plans(plan);
        let mut abandoned_states = Vec::new();
        let mut selected_state = None;

        for (index, attempt_plan) in attempt_plans.iter().enumerate() {
            self.restore_after_error_async(
                instance_mods_dir,
                self.remove_stale_managed(instance_mods_dir, previous_state.as_ref(), attempt_plan),
                previous_state.as_ref(),
            )
            .await
            .map_err(|error| ManagedMutationError::indeterminate("install_reconcile", error))?;

            let state = self
                .attempt_install_plan(
                    attempt_plan,
                    game_version,
                    instance_mods_dir,
                    previous_state.as_ref(),
                )
                .await?;
            let should_fallback =
                severe_install_failure(attempt_plan, &state) && index + 1 < attempt_plans.len();

            if should_fallback {
                let next_plan = &attempt_plans[index + 1];
                warn!(
                    "performance composition {} had severe install failure; trying fallback {}",
                    attempt_plan.composition_id, next_plan.composition_id
                );
                self.restore_after_error_async(
                    instance_mods_dir,
                    self.remove_attempt_mods_not_in_plan(instance_mods_dir, &state, next_plan),
                    previous_state.as_ref(),
                )
                .await
                .map_err(|error| ManagedMutationError::indeterminate("install_fallback", error))?;
                abandoned_states.push(state);
                continue;
            }

            selected_state = Some(state);
            break;
        }

        let state = selected_state.expect("at least the requested performance plan is attempted");
        self.restore_after_error_async(
            instance_mods_dir,
            save_state(instance_mods_dir, &state).map_err(InstallError::State),
            previous_state.as_ref(),
        )
        .await
        .map_err(|error| ManagedMutationError::indeterminate("install_publish", error))?;
        self.settle_added_managed(instance_mods_dir, &state)
            .await
            .map_err(|error| ManagedMutationError::indeterminate("install_settle", error))?;
        self.settle_replaced_managed(instance_mods_dir, previous_state.as_ref(), &state)
            .await
            .map_err(|error| ManagedMutationError::indeterminate("install_settle", error))?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_superseded_managed(instance_mods_dir, previous_state.as_ref(), &state),
            previous_state.as_ref(),
        )
        .await
        .map_err(|error| ManagedMutationError::indeterminate("install_cleanup", error))?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_abandoned_attempt_mods(instance_mods_dir, &abandoned_states, &state),
            previous_state.as_ref(),
        )
        .await
        .map_err(|error| ManagedMutationError::indeterminate("install_cleanup", error))?;
        Ok(state)
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
    ) -> Result<CompositionState, ManagedMutationError> {
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
    ) -> Result<CompositionState, ManagedMutationError> {
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

    fn remove_stale_managed(
        &self,
        instance_mods_dir: &Path,
        state: Option<&CompositionState>,
        plan: &CompositionPlan,
    ) -> Result<(), InstallError> {
        let Some(state) = state else {
            return Ok(());
        };

        let keep: std::collections::HashSet<String> = plan
            .mods
            .iter()
            .map(|managed_mod| managed_mod.project_id.to_lowercase())
            .collect();

        for installed in &state.installed_mods {
            if keep.contains(&installed.project_id.to_lowercase()) {
                continue;
            }
            remove_managed_artifact(instance_mods_dir, installed)?;
        }

        Ok(())
    }

    fn remove_superseded_managed(
        &self,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
        current_state: &CompositionState,
    ) -> Result<(), InstallError> {
        let Some(previous_state) = previous_state else {
            return Ok(());
        };

        let previous_by_project: std::collections::HashMap<String, &InstalledMod> = previous_state
            .installed_mods
            .iter()
            .map(|installed| (installed.project_id.to_lowercase(), installed))
            .collect();

        for installed in &current_state.installed_mods {
            let Some(previous) = previous_by_project.get(&installed.project_id.to_lowercase())
            else {
                continue;
            };
            if previous.filename.is_empty() || previous.filename == installed.filename {
                continue;
            }
            remove_managed_artifact(instance_mods_dir, previous)?;
        }

        Ok(())
    }

    fn remove_attempt_mods_not_in_plan(
        &self,
        instance_mods_dir: &Path,
        state: &CompositionState,
        next_plan: &CompositionPlan,
    ) -> Result<(), InstallError> {
        let keep: std::collections::HashSet<String> = next_plan
            .mods
            .iter()
            .map(|managed_mod| managed_mod.project_id.to_lowercase())
            .collect();

        for installed in &state.installed_mods {
            if keep.contains(&installed.project_id.to_lowercase()) {
                continue;
            }
            remove_managed_artifact(instance_mods_dir, installed)?;
            crate::state::settle_abandoned_managed_artifact_addition(instance_mods_dir, installed)?;
        }

        Ok(())
    }

    fn remove_abandoned_attempt_mods(
        &self,
        instance_mods_dir: &Path,
        abandoned_states: &[CompositionState],
        final_state: &CompositionState,
    ) -> Result<(), InstallError> {
        let keep: std::collections::HashSet<String> = final_state
            .installed_mods
            .iter()
            .map(|installed| installed.filename.clone())
            .collect();

        for state in abandoned_states {
            for installed in &state.installed_mods {
                if keep.contains(&installed.filename) {
                    continue;
                }
                remove_managed_artifact(instance_mods_dir, installed)?;
                crate::state::settle_abandoned_managed_artifact_addition(
                    instance_mods_dir,
                    installed,
                )?;
            }
        }

        Ok(())
    }

    async fn restore_after_error_async<T>(
        &self,
        instance_mods_dir: &Path,
        result: Result<T, InstallError>,
        previous_state: Option<&CompositionState>,
    ) -> Result<T, InstallError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                self.reconcile_replaced_managed(instance_mods_dir, previous_state)
                    .await?;
                if previous_state.is_some() {
                    let snapshot = load_rollback_snapshot_async(instance_mods_dir)
                        .await?
                        .ok_or(InstallError::NoRollbackSnapshot)?;
                    if let Err(rollback_error) =
                        restore_rollback_snapshot_async(instance_mods_dir, &snapshot).await
                    {
                        return Err(InstallError::State(rollback_error));
                    }
                }
                Err(error)
            }
        }
    }

    async fn reconcile_replaced_managed(
        &self,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
    ) -> Result<(), InstallError> {
        for installed in previous_state
            .into_iter()
            .flat_map(|state| state.installed_mods.iter())
        {
            reconcile_managed_replace_backups(
                &instance_mods_dir.join(&installed.filename),
                Some(&installed.integrity.sha512),
            )
            .await?;
        }
        Ok(())
    }

    async fn settle_replaced_managed(
        &self,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
        current_state: &CompositionState,
    ) -> Result<(), InstallError> {
        let previous_by_filename: std::collections::HashMap<&str, &InstalledMod> = previous_state
            .into_iter()
            .flat_map(|state| state.installed_mods.iter())
            .map(|installed| (installed.filename.as_str(), installed))
            .collect();
        for installed in &current_state.installed_mods {
            let Some(previous) = previous_by_filename.get(installed.filename.as_str()) else {
                continue;
            };
            if previous.integrity.sha512 == installed.integrity.sha512 {
                continue;
            }
            settle_managed_replace_backup(
                &instance_mods_dir.join(&installed.filename),
                &previous.integrity.sha512,
                &installed.integrity.sha512,
            )
            .await?;
        }
        Ok(())
    }

    async fn settle_added_managed(
        &self,
        instance_mods_dir: &Path,
        current_state: &CompositionState,
    ) -> Result<(), InstallError> {
        let instance_mods_dir = instance_mods_dir.to_path_buf();
        let current_state = current_state.clone();
        tokio::task::spawn_blocking(move || {
            crate::state::reconcile_managed_addition_obligations(
                &instance_mods_dir,
                Some(&current_state),
            )
        })
        .await
        .map_err(|_| {
            InstallError::Io(std::io::Error::other(
                "managed addition settlement task stopped",
            ))
        })??;
        Ok(())
    }
}

fn classify_rollback_restore_error(error: RollbackRestoreError) -> ManagedMutationError {
    match error {
        RollbackRestoreError::Definite(error) => ManagedMutationError::definite(error),
        RollbackRestoreError::Indeterminate(error) => {
            ManagedMutationError::indeterminate("rollback", error)
        }
    }
}

fn remove_managed_transaction(instance_mods_dir: &Path) -> Result<(), InstallError> {
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
) -> Result<CompositionState, InstallError> {
    let snapshot =
        load_rollback_snapshot(instance_mods_dir)?.ok_or(InstallError::NoRollbackSnapshot)?;
    Ok(restore_rollback_snapshot(instance_mods_dir, &snapshot)?)
}
