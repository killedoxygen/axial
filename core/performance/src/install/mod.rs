use crate::modrinth::{ModrinthClient, ModrinthError};
use crate::resolve::{builtin_manifest, detect_hardware, resolve_plan, validate_manifest};
use crate::rules_cache::{
    RulesCacheStatus, bounded_warning, load_active_rules_cache, write_remote_rules_cache,
};
use crate::state::{
    RollbackSnapshotSummary, StateError, list_rollback_snapshots, load_rollback_snapshot,
    load_rollback_snapshot_by_id, load_state, managed_artifact_path, remove_state,
    restore_rollback_snapshot, save_rollback_snapshot, save_state,
};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::{
    CompositionPlan, CompositionState, InstalledMod, PerformanceMode, ResolutionRequest,
};
use chrono::Utc;
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use thiserror::Error;
use tracing::warn;

pub const PERFORMANCE_RULES_URL_ENV: &str = "CROOPOR_PERFORMANCE_RULES_URL";
const REMOTE_RULES_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RULES_MAX_BYTES: usize = 1024 * 1024;

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
    #[error("performance rollback snapshot not found")]
    RollbackSnapshotNotFound,
}

#[derive(Debug, Error)]
pub enum RulesRefreshError {
    #[error("performance remote rules url is not configured")]
    Unconfigured,
    #[error("remote rules request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("remote rules returned HTTP {0}")]
    HttpStatus(u16),
    #[error("remote rules response is too large")]
    ResponseTooLarge,
    #[error("failed to parse remote performance manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("remote performance manifest failed validation: {0}")]
    Validation(#[from] crate::resolve::ResolveError),
    #[error("failed to persist remote performance manifest: {0}")]
    Cache(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct PerformanceManager {
    active: Arc<RwLock<ActiveRules>>,
    modrinth: ModrinthClient,
    rules_client: reqwest::Client,
    config_dir: Option<PathBuf>,
    remote_rules_url: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveRules {
    manifest: crate::types::Manifest,
    rule_source: RuleSource,
    rule_channel: RuleChannel,
    rules_cache: RulesCacheStatus,
    remote_refresh: bool,
    last_refresh_at: Option<String>,
    validation: RulesValidation,
}

impl PerformanceManager {
    pub fn new() -> Result<Self, InstallError> {
        let manifest = builtin_manifest()?;
        Ok(Self {
            active: Arc::new(RwLock::new(ActiveRules {
                manifest,
                rule_source: RuleSource::BuiltIn,
                rule_channel: RuleChannel::Bundled,
                rules_cache: RulesCacheStatus::unavailable(),
                remote_refresh: false,
                last_refresh_at: None,
                validation: RulesValidation::Valid,
            })),
            modrinth: ModrinthClient::new(),
            rules_client: rules_client(),
            config_dir: None,
            remote_rules_url: None,
        })
    }

    pub fn new_with_config_dir(config_dir: &Path) -> Result<Self, InstallError> {
        Self::new_with_config_dir_and_remote_url(config_dir, configured_remote_rules_url())
    }

    pub fn new_with_config_dir_and_remote_url(
        config_dir: &Path,
        remote_rules_url: Option<String>,
    ) -> Result<Self, InstallError> {
        let manifest = builtin_manifest()?;
        let remote_rules_url = normalize_remote_rules_url(remote_rules_url);
        let loaded = load_active_rules_cache(config_dir, &manifest, remote_rules_url.is_some());
        Ok(Self {
            active: Arc::new(RwLock::new(ActiveRules {
                manifest: loaded.manifest,
                rule_source: loaded.rule_source,
                rule_channel: loaded.rule_channel,
                rules_cache: loaded.status,
                remote_refresh: remote_rules_url.is_some(),
                last_refresh_at: loaded.last_refresh_at,
                validation: loaded.validation,
            })),
            modrinth: ModrinthClient::new(),
            rules_client: rules_client(),
            config_dir: Some(config_dir.to_path_buf()),
            remote_rules_url,
        })
    }

    pub fn get_plan(&self, request: ResolutionRequest) -> CompositionPlan {
        let active = self.active.read().expect("performance rules lock poisoned");
        resolve_plan(Some(&active.manifest), request)
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

    pub fn manifest(&self) -> crate::types::Manifest {
        self.active
            .read()
            .expect("performance rules lock poisoned")
            .manifest
            .clone()
    }

    pub fn rules_status(&self) -> crate::status::PerformanceRulesStatus {
        let active = self.active.read().expect("performance rules lock poisoned");
        crate::status::rules_status_for(
            &active.manifest,
            active.rule_source,
            active.rule_channel,
            active.rules_cache.clone(),
            active.remote_refresh,
            active.last_refresh_at.clone(),
            active.validation,
        )
    }

    pub async fn refresh_rules(
        &self,
    ) -> Result<crate::status::PerformanceRulesStatus, RulesRefreshError> {
        let Some(config_dir) = self.config_dir.as_ref() else {
            return Err(RulesRefreshError::Unconfigured);
        };
        let Some(remote_rules_url) = self.remote_rules_url.as_ref() else {
            return Err(RulesRefreshError::Unconfigured);
        };

        match self.fetch_remote_manifest(remote_rules_url).await {
            Ok(manifest) => match self.accept_remote_manifest(config_dir, manifest) {
                Ok(()) => Ok(self.rules_status()),
                Err(error) => {
                    self.record_refresh_warning(format!("Remote rules refresh failed: {error}"));
                    Ok(self.rules_status())
                }
            },
            Err(error) => {
                self.record_refresh_warning(format!("Remote rules refresh rejected: {error}"));
                Ok(self.rules_status())
            }
        }
    }

    pub fn hardware(&self) -> crate::types::HardwareProfile {
        detect_hardware()
    }

    async fn fetch_remote_manifest(
        &self,
        remote_rules_url: &str,
    ) -> Result<crate::types::Manifest, RulesRefreshError> {
        let response = self
            .rules_client
            .get(remote_rules_url)
            .timeout(REMOTE_RULES_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(RulesRefreshError::HttpStatus(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > REMOTE_RULES_MAX_BYTES as u64)
        {
            return Err(RulesRefreshError::ResponseTooLarge);
        }

        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await? {
            if body.len().saturating_add(chunk.len()) > REMOTE_RULES_MAX_BYTES {
                return Err(RulesRefreshError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }

        let manifest = serde_json::from_slice::<crate::types::Manifest>(&body)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn accept_remote_manifest(
        &self,
        config_dir: &Path,
        manifest: crate::types::Manifest,
    ) -> Result<(), RulesRefreshError> {
        validate_manifest(&manifest)?;
        let rules_cache = write_remote_rules_cache(config_dir, &manifest)?;
        let last_refresh_at = rules_cache.updated_at.clone();
        let mut active = self
            .active
            .write()
            .expect("performance rules lock poisoned");
        *active = ActiveRules {
            manifest,
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            rules_cache,
            remote_refresh: true,
            last_refresh_at,
            validation: RulesValidation::Valid,
        };
        Ok(())
    }

    fn record_refresh_warning(&self, warning: String) {
        let mut active = self
            .active
            .write()
            .expect("performance rules lock poisoned");
        active.rules_cache.warning = Some(bounded_warning(warning));
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

fn configured_remote_rules_url() -> Option<String> {
    normalize_remote_rules_url(std::env::var(PERFORMANCE_RULES_URL_ENV).ok())
}

fn normalize_remote_rules_url(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn rules_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("croopor/0.3.1 performance-rules")
        .timeout(REMOTE_RULES_TIMEOUT)
        .build()
        .expect("build performance rules client")
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
                "id": "rb-path-traversal",
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

    #[test]
    fn startup_uses_valid_cached_remote_rules_when_url_is_configured() {
        let root = test_root("startup-remote-cache");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.generated_at = "2026-05-30T11:00:00Z".to_string();
        write_remote_rules_cache(&root, &remote).expect("write remote cache");

        let manager = PerformanceManager::new_with_config_dir_and_remote_url(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
        )
        .expect("performance manager");
        let status = manager.rules_status();

        assert_eq!(status.rule_source, RuleSource::Remote);
        assert_eq!(status.rule_channel, RuleChannel::Remote);
        assert!(status.remote_refresh);
        assert!(status.last_refresh_at.is_some());
        assert_eq!(status.generated_at, remote.generated_at);
        assert_eq!(status.validation, RulesValidation::Valid);
        assert!(status.warnings.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accepted_remote_refresh_persists_and_updates_active_status() {
        let root = test_root("accept-remote-refresh");
        let manager = PerformanceManager::new_with_config_dir_and_remote_url(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
        )
        .expect("performance manager");
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-05-30T12:00:00Z".to_string();

        manager
            .accept_remote_manifest(&root, remote.clone())
            .expect("accept remote manifest");
        let status = manager.rules_status();

        assert_eq!(status.rule_source, RuleSource::Remote);
        assert_eq!(status.rule_channel, RuleChannel::Remote);
        assert_eq!(status.generated_at, remote.generated_at);
        assert!(status.last_refresh_at.is_some());

        let reloaded = crate::rules_cache::load_active_rules_cache(
            &root,
            &builtin_manifest().expect("builtin manifest"),
            true,
        );
        assert_eq!(reloaded.rule_source, RuleSource::Remote);
        assert_eq!(reloaded.manifest.generated_at, remote.generated_at);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_remote_refresh_keeps_previous_rules_and_exposes_warning() {
        let root = test_root("reject-remote-refresh");
        let manager = PerformanceManager::new_with_config_dir_and_remote_url(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
        )
        .expect("performance manager");
        let before = manager.rules_status();
        let mut invalid = manager.manifest();
        invalid.schema_version = 99;

        let error = manager
            .accept_remote_manifest(&root, invalid)
            .expect_err("invalid remote manifest should be rejected");
        manager.record_refresh_warning(format!("Remote rules refresh rejected: {error}"));
        let after = manager.rules_status();

        assert_eq!(after.rule_source, before.rule_source);
        assert_eq!(after.rule_channel, before.rule_channel);
        assert_eq!(after.generated_at, before.generated_at);
        assert_eq!(after.validation, RulesValidation::Valid);
        assert!(
            after
                .warnings
                .iter()
                .any(|warning| warning.contains("Remote rules refresh rejected"))
        );

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
