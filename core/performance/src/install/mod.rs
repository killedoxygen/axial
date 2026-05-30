use crate::modrinth::{ModrinthClient, ModrinthError};
use crate::resolve::{builtin_manifest, detect_hardware, resolve_plan};
use crate::rules_cache::{RulesCacheStatus, load_or_repair_rules_cache};
use crate::state::{
    StateError, load_rollback_snapshot, load_state, managed_artifact_path, remove_state,
    restore_rollback_snapshot, save_rollback_snapshot, save_state,
};
use crate::types::{
    CompositionPlan, CompositionState, InstalledMod, PerformanceMode, ResolutionRequest,
};
use chrono::Utc;
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("failed to load performance manifest: {0}")]
    Manifest(#[from] crate::resolve::ResolveError),
    #[error("failed to access performance state: {0}")]
    State(#[from] StateError),
    #[error("modrinth error: {0}")]
    Modrinth(#[from] ModrinthError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("composition plan is required")]
    MissingPlan,
    #[error("no compatible versions found for {0}")]
    NoCompatibleVersion(String),
    #[error("no downloadable file for {0}")]
    NoPrimaryFile(String),
    #[error("mod filename is invalid: {0}")]
    InvalidFilename(String),
    #[error("no performance rollback snapshot available")]
    NoRollbackSnapshot,
}

#[derive(Debug, Clone)]
pub struct PerformanceManager {
    manifest: crate::types::Manifest,
    modrinth: ModrinthClient,
    rules_cache: RulesCacheStatus,
}

impl PerformanceManager {
    pub fn new() -> Result<Self, InstallError> {
        Ok(Self {
            manifest: builtin_manifest()?,
            modrinth: ModrinthClient::new(),
            rules_cache: RulesCacheStatus::unavailable(),
        })
    }

    pub fn new_with_config_dir(config_dir: &Path) -> Result<Self, InstallError> {
        let manifest = builtin_manifest()?;
        let rules_cache = load_or_repair_rules_cache(config_dir, &manifest);
        Ok(Self {
            manifest,
            modrinth: ModrinthClient::new(),
            rules_cache,
        })
    }

    pub fn get_plan(&self, request: ResolutionRequest) -> CompositionPlan {
        resolve_plan(Some(&self.manifest), request)
    }

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
            save_rollback_snapshot(instance_mods_dir, previous_state)?;
        }
        self.restore_after_error(
            instance_mods_dir,
            self.remove_stale_managed(instance_mods_dir, previous_state.as_ref(), plan),
            snapshot_available,
        )?;

        let mut state = CompositionState {
            composition_id: plan.composition_id.clone(),
            tier: plan.tier,
            installed_mods: Vec::with_capacity(plan.mods.len()),
            installed_at: Utc::now().to_rfc3339(),
            failure_count: 0,
            last_failure: String::new(),
        };

        for managed_mod in &plan.mods {
            match self
                .install_mod(managed_mod, game_version, &plan.loader, instance_mods_dir)
                .await
            {
                Ok(installed) => state.installed_mods.push(installed),
                Err(error) => {
                    state.failure_count += 1;
                    state.last_failure = error.to_string();
                    warn!(
                        "performance install failed for {}: {}",
                        managed_mod.project_id, error
                    );
                }
            }
        }

        state
            .installed_mods
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        self.restore_after_error(
            instance_mods_dir,
            save_state(instance_mods_dir, &state).map_err(InstallError::State),
            snapshot_available,
        )?;
        self.restore_after_error(
            instance_mods_dir,
            self.remove_superseded_managed(instance_mods_dir, previous_state.as_ref(), &state),
            snapshot_available,
        )?;
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

    pub fn manifest(&self) -> &crate::types::Manifest {
        &self.manifest
    }

    pub fn rules_status(&self) -> crate::status::PerformanceRulesStatus {
        crate::status::rules_status_with_cache(&self.manifest, self.rules_cache.clone())
    }

    pub fn hardware(&self) -> crate::types::HardwareProfile {
        detect_hardware()
    }

    async fn install_mod(
        &self,
        managed_mod: &crate::types::ManagedMod,
        game_version: &str,
        loader: &str,
        instance_mods_dir: &Path,
    ) -> Result<InstalledMod, InstallError> {
        let game_versions = vec![game_version.to_string()];
        let loaders = vec![loader.to_string()];

        let mut versions = self
            .modrinth
            .list_versions(&managed_mod.project_id, &game_versions, &loaders)
            .await?;
        if versions.is_empty()
            && let Some(parent_minor) = parent_minor_version(game_version)
            && parent_minor != game_version
        {
            versions = self
                .modrinth
                .list_versions(&managed_mod.project_id, &[parent_minor], &loaders)
                .await?;
        }

        let version = versions
            .into_iter()
            .next()
            .ok_or_else(|| InstallError::NoCompatibleVersion(managed_mod.project_id.clone()))?;
        let file = version
            .primary_file()
            .ok_or_else(|| InstallError::NoPrimaryFile(managed_mod.project_id.clone()))?;
        let filename = sanitize_mod_filename(&file.filename)?;
        let expected_sha = file.hashes.get("sha512").cloned().unwrap_or_default();
        let final_path = instance_mods_dir.join(&filename);

        if !expected_sha.is_empty()
            && let Ok(existing) = fs::read(&final_path)
        {
            let actual = hex::encode(sha2::Sha512::digest(&existing));
            if actual.eq_ignore_ascii_case(&expected_sha) {
                return Ok(InstalledMod {
                    project_id: managed_mod.project_id.clone(),
                    version_id: version.id,
                    filename,
                    sha512: expected_sha,
                });
            }
        }

        let bytes = self
            .modrinth
            .download_file(&file.url, &expected_sha)
            .await?;
        let temp_path = PathBuf::from(format!("{}.tmp", final_path.display()));
        fs::write(&temp_path, bytes)?;
        replace_file_atomic(&temp_path, &final_path)?;

        Ok(InstalledMod {
            project_id: managed_mod.project_id.clone(),
            version_id: version.id,
            filename,
            sha512: expected_sha,
        })
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
}

fn empty_state(plan: &CompositionPlan) -> CompositionState {
    CompositionState {
        composition_id: plan.composition_id.clone(),
        tier: plan.tier,
        installed_mods: Vec::new(),
        installed_at: Utc::now().to_rfc3339(),
        failure_count: 0,
        last_failure: String::new(),
    }
}

fn parent_minor_version(game_version: &str) -> Option<String> {
    let mut parts = game_version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    Some(format!("{major}.{minor}"))
}

fn sanitize_mod_filename(name: &str) -> Result<String, InstallError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(InstallError::InvalidFilename(name.to_string()));
    }
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| InstallError::InvalidFilename(name.to_string()))?;
    if base != trimmed {
        return Err(InstallError::InvalidFilename(name.to_string()));
    }
    Ok(base.to_string())
}

fn replace_file_atomic(temp_path: &Path, final_path: &Path) -> Result<(), std::io::Error> {
    if fs::rename(temp_path, final_path).is_ok() {
        return Ok(());
    }
    if final_path.exists() {
        let _ = fs::remove_file(final_path);
    }
    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temp_path);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{load_state, save_state};
    use crate::types::CompositionTier;

    #[test]
    fn rollback_restores_previous_managed_files_without_touching_user_files() {
        let root = test_root("rollback-restores-managed");
        let manager = PerformanceManager::new().expect("performance manager");
        fs::write(root.join("managed.jar"), b"managed-v1").expect("write managed file");
        fs::write(root.join("user.jar"), b"user-v1").expect("write user file");
        save_state(
            &root,
            &test_state("core", vec![test_mod("sodium", "managed.jar")]),
        )
        .expect("save state");

        manager
            .remove_managed(&root)
            .expect("remove managed bundle");
        fs::write(root.join("user.jar"), b"user-v2").expect("mutate user file");

        let restored = manager
            .rollback_managed(&root)
            .expect("rollback should restore latest snapshot");

        assert_eq!(restored.composition_id, "core");
        assert_eq!(
            fs::read(root.join("managed.jar")).expect("read managed"),
            b"managed-v1"
        );
        assert_eq!(
            fs::read(root.join("user.jar")).expect("read user"),
            b"user-v2"
        );
        assert_eq!(
            load_state(&root)
                .expect("load state")
                .expect("state restored")
                .installed_mods
                .len(),
            1
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_without_snapshot_is_predictable() {
        let root = test_root("rollback-missing");
        let manager = PerformanceManager::new().expect("performance manager");

        let error = manager
            .rollback_managed(&root)
            .expect_err("missing snapshot should fail");

        assert!(matches!(error, InstallError::NoRollbackSnapshot));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_rejects_path_traversal_metadata() {
        let root = test_root("rollback-path-traversal");
        let manager = PerformanceManager::new().expect("performance manager");
        let rollback_dir = root.join(".croopor-performance").join("rollback");
        fs::create_dir_all(&rollback_dir).expect("create rollback dir");
        fs::write(
            rollback_dir.join("latest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "created_at": "2026-05-30T00:00:00Z",
                "state": test_state("core", vec![test_mod("sodium", "../outside.jar")]),
                "artifacts": []
            }))
            .expect("serialize snapshot"),
        )
        .expect("write snapshot");

        let error = manager
            .rollback_managed(&root)
            .expect_err("traversal metadata should fail");

        assert!(matches!(
            error,
            InstallError::State(StateError::InvalidFilename(_))
        ));
        assert!(!root.join("..").join("outside.jar").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hard_remove_error_restores_deleted_managed_file() {
        let root = test_root("rollback-after-remove-error");
        let manager = PerformanceManager::new().expect("performance manager");
        fs::write(root.join("managed.jar"), b"managed-v1").expect("write managed file");
        fs::create_dir(root.join("blocked.jar")).expect("create blocking directory");
        save_state(
            &root,
            &test_state(
                "core",
                vec![
                    test_mod("sodium", "managed.jar"),
                    test_mod("lithium", "blocked.jar"),
                ],
            ),
        )
        .expect("save state");

        let error = manager
            .remove_managed(&root)
            .expect_err("directory removal should fail");

        assert!(matches!(error, InstallError::Io(_)));
        assert_eq!(
            fs::read(root.join("managed.jar")).expect("read managed"),
            b"managed-v1"
        );
        assert!(load_state(&root).expect("load state").is_some());
        let _ = fs::remove_dir_all(root);
    }

    fn test_state(composition_id: &str, installed_mods: Vec<InstalledMod>) -> CompositionState {
        CompositionState {
            composition_id: composition_id.to_string(),
            tier: CompositionTier::Core,
            installed_mods,
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        }
    }

    fn test_mod(project_id: &str, filename: &str) -> InstalledMod {
        InstalledMod {
            project_id: project_id.to_string(),
            version_id: "version".to_string(),
            filename: filename.to_string(),
            sha512: String::new(),
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-performance-install-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }
}
