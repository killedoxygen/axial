use super::fallback::{empty_state, severe_install_failure};
use super::manager::PerformanceManager;
use super::model::InstallError;
use super::promotion::{reconcile_managed_replace_backups, settle_managed_replace_backup};
use crate::state::{
    RollbackSnapshotSummary, list_rollback_snapshots_async, load_rollback_snapshot,
    load_rollback_snapshot_async, load_rollback_snapshot_by_id_async, load_state,
    remove_managed_artifact, remove_state, restore_rollback_snapshot,
    restore_rollback_snapshot_async, save_rollback_snapshot, save_rollback_snapshot_async,
    save_state, settle_managed_artifact_removal, stage_managed_artifact_removal,
};
use crate::types::{CompositionPlan, CompositionState, InstalledMod, PerformanceMode};
use std::fs;
use std::path::Path;
use tracing::warn;

impl PerformanceManager {
    pub async fn ensure_installed(
        &self,
        plan: &CompositionPlan,
        game_version: &str,
        instance_mods_dir: &Path,
    ) -> Result<CompositionState, InstallError> {
        if !matches!(plan.mode, PerformanceMode::Managed) {
            self.remove_managed_async(instance_mods_dir).await?;
            return Ok(empty_state(plan));
        }

        fs::create_dir_all(instance_mods_dir)?;
        let previous_state = load_state(instance_mods_dir)?;
        if let Some(previous_state) = previous_state.as_ref() {
            save_rollback_snapshot_async(instance_mods_dir, previous_state).await?;
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
            .await?;

            let state = self
                .attempt_install_plan(
                    attempt_plan,
                    game_version,
                    instance_mods_dir,
                    previous_state.as_ref(),
                )
                .await;
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
                .await?;
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
        .await?;
        self.settle_replaced_managed(instance_mods_dir, previous_state.as_ref(), &state)
            .await?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_superseded_managed(instance_mods_dir, previous_state.as_ref(), &state),
            previous_state.as_ref(),
        )
        .await?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_abandoned_attempt_mods(instance_mods_dir, &abandoned_states, &state),
            previous_state.as_ref(),
        )
        .await?;
        Ok(state)
    }

    pub async fn remove_managed_async(&self, instance_mods_dir: &Path) -> Result<(), InstallError> {
        let instance_mods_dir = instance_mods_dir.to_path_buf();
        tokio::task::spawn_blocking(move || remove_managed_transaction(&instance_mods_dir))
            .await
            .map_err(|_| {
                InstallError::Io(std::io::Error::other(
                    "managed performance removal task stopped before reporting its result",
                ))
            })?
    }

    pub async fn rollback_managed_async(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<CompositionState, InstallError> {
        let snapshot = load_rollback_snapshot_async(instance_mods_dir)
            .await?
            .ok_or(InstallError::NoRollbackSnapshot)?;
        Ok(restore_rollback_snapshot_async(instance_mods_dir, &snapshot).await?)
    }

    pub async fn rollback_managed_snapshot_async(
        &self,
        instance_mods_dir: &Path,
        snapshot_id: &str,
    ) -> Result<CompositionState, InstallError> {
        let snapshot = load_rollback_snapshot_by_id_async(instance_mods_dir, snapshot_id)
            .await?
            .ok_or(InstallError::RollbackSnapshotNotFound)?;
        Ok(restore_rollback_snapshot_async(instance_mods_dir, &snapshot).await?)
    }

    pub async fn list_rollback_snapshots_async(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<Vec<RollbackSnapshotSummary>, InstallError> {
        Ok(list_rollback_snapshots_async(instance_mods_dir).await?)
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
                if previous_state.is_some()
                    && let Err(rollback_error) =
                        self.rollback_managed_async(instance_mods_dir).await
                {
                    return Err(rollback_error);
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
