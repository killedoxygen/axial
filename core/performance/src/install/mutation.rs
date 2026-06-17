use super::fallback::{empty_state, severe_install_failure};
use super::manager::PerformanceManager;
use super::model::InstallError;
use crate::state::{
    RollbackSnapshotSummary, list_rollback_snapshots, load_rollback_snapshot,
    load_rollback_snapshot_async, load_rollback_snapshot_by_id, load_state, managed_artifact_path,
    remove_state, restore_rollback_snapshot, restore_rollback_snapshot_async,
    save_rollback_snapshot, save_rollback_snapshot_async, save_state,
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
            self.remove_managed(instance_mods_dir)?;
            return Ok(empty_state(plan));
        }

        fs::create_dir_all(instance_mods_dir)?;
        let previous_state = load_state(instance_mods_dir)?;
        let snapshot_available = previous_state.is_some();
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
                snapshot_available,
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
                    snapshot_available,
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
            snapshot_available,
        )
        .await?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_superseded_managed(instance_mods_dir, previous_state.as_ref(), &state),
            snapshot_available,
        )
        .await?;
        self.restore_after_error_async(
            instance_mods_dir,
            self.remove_abandoned_attempt_mods(instance_mods_dir, &abandoned_states, &state),
            snapshot_available,
        )
        .await?;
        Ok(state)
    }

    pub fn remove_managed(&self, instance_mods_dir: &Path) -> Result<(), InstallError> {
        if let Some(state) = load_state(instance_mods_dir)? {
            save_rollback_snapshot(instance_mods_dir, &state)?;
            let result = (|| -> Result<(), InstallError> {
                for installed in &state.installed_mods {
                    let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
                    if let Err(error) = fs::remove_file(path)
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(InstallError::Io(error));
                    }
                }
                remove_state(instance_mods_dir)?;
                Ok(())
            })();
            self.restore_after_error(instance_mods_dir, result, true)?;
        }
        Ok(())
    }

    pub fn rollback_managed(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<CompositionState, InstallError> {
        let snapshot =
            load_rollback_snapshot(instance_mods_dir)?.ok_or(InstallError::NoRollbackSnapshot)?;
        Ok(restore_rollback_snapshot(instance_mods_dir, &snapshot)?)
    }

    async fn rollback_managed_async(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<CompositionState, InstallError> {
        let snapshot = load_rollback_snapshot_async(instance_mods_dir)
            .await?
            .ok_or(InstallError::NoRollbackSnapshot)?;
        Ok(restore_rollback_snapshot_async(instance_mods_dir, &snapshot).await?)
    }

    pub fn rollback_managed_snapshot(
        &self,
        instance_mods_dir: &Path,
        snapshot_id: &str,
    ) -> Result<CompositionState, InstallError> {
        let snapshot = load_rollback_snapshot_by_id(instance_mods_dir, snapshot_id)?
            .ok_or(InstallError::RollbackSnapshotNotFound)?;
        Ok(restore_rollback_snapshot(instance_mods_dir, &snapshot)?)
    }

    pub fn list_rollback_snapshots(
        &self,
        instance_mods_dir: &Path,
    ) -> Result<Vec<RollbackSnapshotSummary>, InstallError> {
        Ok(list_rollback_snapshots(instance_mods_dir)?)
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
            let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(InstallError::Io(error));
            }
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
            let path = managed_artifact_path(instance_mods_dir, &previous.filename)?;
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(InstallError::Io(error));
            }
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
            let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(InstallError::Io(error));
            }
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
                let path = managed_artifact_path(instance_mods_dir, &installed.filename)?;
                if let Err(error) = fs::remove_file(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(InstallError::Io(error));
                }
            }
        }

        Ok(())
    }

    fn restore_after_error<T>(
        &self,
        instance_mods_dir: &Path,
        result: Result<T, InstallError>,
        snapshot_available: bool,
    ) -> Result<T, InstallError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if snapshot_available
                    && let Err(rollback_error) = self.rollback_managed(instance_mods_dir)
                {
                    warn!(
                        "failed to restore performance rollback snapshot after error: {}",
                        rollback_error
                    );
                }
                Err(error)
            }
        }
    }

    async fn restore_after_error_async<T>(
        &self,
        instance_mods_dir: &Path,
        result: Result<T, InstallError>,
        snapshot_available: bool,
    ) -> Result<T, InstallError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if snapshot_available
                    && let Err(rollback_error) =
                        self.rollback_managed_async(instance_mods_dir).await
                {
                    warn!(
                        "failed to restore performance rollback snapshot after error: {}",
                        rollback_error
                    );
                }
                Err(error)
            }
        }
    }
}
