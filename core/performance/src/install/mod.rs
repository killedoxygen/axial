use crate::modrinth::{ModrinthClient, ModrinthError, Version};
use crate::resolve::{builtin_manifest, detect_hardware, resolve_plan, validate_manifest};
use crate::rules_cache::{
    RulesCacheStatus, bounded_warning, load_active_rules_cache, write_remote_rules_cache,
};
use crate::signature::{
    RULES_KEY_ID_HEADER, RULES_SIGNATURE_HEADER, RemoteRulesVerifier, RulesSignatureError,
    RulesSignatureMetadata, configured_remote_rules_verifier, signature_metadata_from_header,
};
use crate::state::{
    RollbackSnapshotSummary, StateError, list_rollback_snapshots, load_rollback_snapshot,
    load_rollback_snapshot_by_id, load_state, managed_artifact_path, remove_state,
    restore_rollback_snapshot, save_rollback_snapshot, save_state,
};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::{
    CompositionPlan, CompositionState, InstalledMod, ManagedArtifactIntegrity,
    ManagedArtifactProvider, ManagedArtifactSource, OwnershipClass, PerformanceMode,
    ResolutionRequest,
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
    #[error("{0}")]
    Signature(#[from] RulesSignatureError),
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
    remote_rules_verifier: RemoteRulesVerifier,
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
            remote_rules_verifier: RemoteRulesVerifier::disabled(),
        })
    }

    pub fn new_with_config_dir(config_dir: &Path) -> Result<Self, InstallError> {
        Self::new_with_config_dir_and_remote_url(config_dir, configured_remote_rules_url())
    }

    pub fn new_with_config_dir_and_remote_url(
        config_dir: &Path,
        remote_rules_url: Option<String>,
    ) -> Result<Self, InstallError> {
        Self::new_with_config_dir_remote_url_and_public_key(
            config_dir,
            remote_rules_url,
            std::env::var(crate::signature::PERFORMANCE_RULES_PUBLIC_KEY_ENV).ok(),
        )
    }

    pub fn new_with_config_dir_remote_url_and_public_key(
        config_dir: &Path,
        remote_rules_url: Option<String>,
        remote_rules_public_key: Option<String>,
    ) -> Result<Self, InstallError> {
        let manifest = builtin_manifest()?;
        let remote_rules_url = normalize_remote_rules_url(remote_rules_url);
        let remote_rules_verifier = if remote_rules_url.is_some() {
            RemoteRulesVerifier::from_public_key_hex(remote_rules_public_key)
        } else {
            configured_remote_rules_verifier(false)
        };
        let loaded = load_active_rules_cache(
            config_dir,
            &manifest,
            remote_rules_url.is_some(),
            &remote_rules_verifier,
        );
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
            remote_rules_verifier,
        })
    }

    #[cfg(test)]
    fn new_with_modrinth_base_url(base_url: String) -> Result<Self, InstallError> {
        let mut manager = Self::new()?;
        manager.modrinth = ModrinthClient::new_with_base_url(base_url);
        Ok(manager)
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

    pub fn remote_refresh_enabled(&self) -> bool {
        self.remote_rules_url.is_some()
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
        if let Some(warning) = self.remote_rules_verifier.acceptance_warning() {
            self.record_refresh_warning(format!("Remote rules refresh rejected: {warning}"));
            return Ok(self.rules_status());
        }

        match self.fetch_remote_manifest(remote_rules_url).await {
            Ok(candidate) => match self.accept_remote_manifest(config_dir, candidate) {
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
    ) -> Result<RemoteRulesCandidate, RulesRefreshError> {
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
        let signature = signature_metadata_from_header(
            response
                .headers()
                .get(RULES_SIGNATURE_HEADER)
                .and_then(|value| value.to_str().ok()),
            response
                .headers()
                .get(RULES_KEY_ID_HEADER)
                .and_then(|value| value.to_str().ok()),
        )?;

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
        self.remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        Ok(RemoteRulesCandidate {
            manifest,
            signature,
        })
    }

    fn accept_remote_manifest(
        &self,
        config_dir: &Path,
        candidate: RemoteRulesCandidate,
    ) -> Result<(), RulesRefreshError> {
        let RemoteRulesCandidate {
            manifest,
            signature,
        } = candidate;
        validate_manifest(&manifest)?;
        self.remote_rules_verifier
            .verify_manifest(&manifest, &signature)?;
        let rules_cache = write_remote_rules_cache(config_dir, &manifest, signature)?;
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
        let loaders = vec![loader.to_string()];

        let versions = self
            .resolve_managed_mod_versions(managed_mod, game_version, &loaders)
            .await?;
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
                    ownership_class: OwnershipClass::CompositionManaged,
                    source: modrinth_source(),
                    integrity: verified_sha512_integrity(expected_sha),
                });
            }
        }

        let temp_path = PathBuf::from(format!("{}.tmp", final_path.display()));
        self.modrinth
            .download_file_to_path(&file.url, &expected_sha, &temp_path)
            .await?;
        replace_file_atomic(&temp_path, &final_path)?;

        Ok(InstalledMod {
            project_id: managed_mod.project_id.clone(),
            version_id: version.id,
            filename,
            ownership_class: OwnershipClass::CompositionManaged,
            source: modrinth_source(),
            integrity: if expected_sha.trim().is_empty() {
                unverified_sha512_integrity(expected_sha)
            } else {
                verified_sha512_integrity(expected_sha)
            },
        })
    }

    async fn resolve_managed_mod_versions(
        &self,
        managed_mod: &crate::types::ManagedMod,
        game_version: &str,
        loaders: &[String],
    ) -> Result<Vec<Version>, InstallError> {
        let project_result = self
            .list_versions_with_game_fallback(&managed_mod.project_id, game_version, loaders)
            .await;

        match project_result {
            Ok(versions) if !versions.is_empty() => Ok(versions),
            Ok(_) => self
                .list_versions_with_game_fallback(&managed_mod.slug, game_version, loaders)
                .await
                .map_err(InstallError::Modrinth),
            Err(ModrinthError::Http { status: 404, .. }) => self
                .list_versions_with_game_fallback(&managed_mod.slug, game_version, loaders)
                .await
                .map_err(InstallError::Modrinth),
            Err(error) => Err(InstallError::Modrinth(error)),
        }
    }

    async fn list_versions_with_game_fallback(
        &self,
        project_ref: &str,
        game_version: &str,
        loaders: &[String],
    ) -> Result<Vec<Version>, ModrinthError> {
        let game_versions = vec![game_version.to_string()];
        let mut versions = self
            .modrinth
            .list_versions(project_ref, &game_versions, loaders)
            .await?;
        if versions.is_empty()
            && let Some(parent_minor) = parent_minor_version(game_version)
            && parent_minor != game_version
        {
            versions = self
                .modrinth
                .list_versions(project_ref, &[parent_minor], loaders)
                .await?;
        }
        Ok(versions)
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

#[derive(Debug, Clone)]
struct RemoteRulesCandidate {
    manifest: crate::types::Manifest,
    signature: RulesSignatureMetadata,
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

fn modrinth_source() -> ManagedArtifactSource {
    ManagedArtifactSource {
        provider: ManagedArtifactProvider::Modrinth,
    }
}

fn verified_sha512_integrity(sha512: String) -> ManagedArtifactIntegrity {
    ManagedArtifactIntegrity {
        sha512,
        sha512_verified: true,
    }
}

fn unverified_sha512_integrity(sha512: String) -> ManagedArtifactIntegrity {
    ManagedArtifactIntegrity {
        sha512,
        sha512_verified: false,
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
    use crate::types::{CompositionTier, ManagedMod, ModCondition, VersionFamily};
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn ensure_installed_writes_composition_managed_ownership() {
        let root = test_root("ensure-installed-ownership");
        let manager =
            PerformanceManager::new_with_modrinth_base_url(spawn_modrinth_server(false).await)
                .expect("performance manager");
        let plan = CompositionPlan {
            composition_id: "core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![ManagedMod {
                artifact_id: "sodium".to_string(),
                project_id: "sodium".to_string(),
                slug: "sodium".to_string(),
                name: "Sodium".to_string(),
                condition: ModCondition::Always,
                version_range: String::new(),
                hardware_req: None,
                mutual_exclusions: Vec::new(),
            }],
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install managed artifact");

        assert_eq!(state.installed_mods.len(), 1);
        assert_eq!(
            state.installed_mods[0].ownership_class,
            OwnershipClass::CompositionManaged
        );
        assert_eq!(
            state.installed_mods[0].source.provider,
            ManagedArtifactProvider::Modrinth
        );
        assert!(!state.installed_mods[0].integrity.sha512_verified);
        assert!(root.join("sodium.jar").is_file());
        let loaded = load_state(&root)
            .expect("load state")
            .expect("state should exist");
        assert_eq!(
            loaded.installed_mods[0].ownership_class,
            OwnershipClass::CompositionManaged
        );
        assert_eq!(
            loaded.installed_mods[0].source.provider,
            ManagedArtifactProvider::Modrinth
        );
        assert!(!loaded.installed_mods[0].integrity.sha512_verified);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_installed_records_verified_modrinth_sha512_when_available() {
        let root = test_root("ensure-installed-verified-sha512");
        let manager =
            PerformanceManager::new_with_modrinth_base_url(spawn_modrinth_server(true).await)
                .expect("performance manager");
        let plan = CompositionPlan {
            composition_id: "core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![ManagedMod {
                artifact_id: "sodium".to_string(),
                project_id: "sodium".to_string(),
                slug: "sodium".to_string(),
                name: "Sodium".to_string(),
                condition: ModCondition::Always,
                version_range: String::new(),
                hardware_req: None,
                mutual_exclusions: Vec::new(),
            }],
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install managed artifact");

        assert_eq!(state.installed_mods.len(), 1);
        assert_eq!(
            state.installed_mods[0].source.provider,
            ManagedArtifactProvider::Modrinth
        );
        assert!(state.installed_mods[0].integrity.sha512_verified);
        assert!(!state.installed_mods[0].integrity.sha512.is_empty());
        assert_eq!(
            fs::read(root.join("sodium.jar")).expect("read verified file"),
            b"managed-jar"
        );
        assert!(!root.join("sodium.jar.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_install_uses_project_id_without_slug_fallback() {
        let (base_url, requests) =
            spawn_modrinth_identity_server(ProjectLookupResponse::Version).await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
        let managed_mod = managed_mod("declared-project", "declared-slug");
        let loaders = vec!["fabric".to_string()];

        let versions = manager
            .resolve_managed_mod_versions(&managed_mod, "1.20.4", &loaders)
            .await
            .expect("resolve managed artifact by project id");

        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id, "declared-project-version");
        let requests = requests.lock().expect("request log").clone();
        assert!(request_log_contains(
            &requests,
            "/v2/project/declared-project/version"
        ));
        assert!(!request_log_contains(
            &requests,
            "/v2/project/declared-slug/version"
        ));
    }

    #[tokio::test]
    async fn managed_install_falls_back_to_slug_after_project_id_404_or_no_compatible_version() {
        for (name, response) in [
            ("project-id-404", ProjectLookupResponse::NotFound),
            ("project-id-empty", ProjectLookupResponse::Empty),
        ] {
            let (base_url, requests) = spawn_modrinth_identity_server(response).await;
            let manager = PerformanceManager::new_with_modrinth_base_url(base_url)
                .expect("performance manager");
            let managed_mod = managed_mod("declared-project", "declared-slug");
            let loaders = vec!["fabric".to_string()];

            let versions = manager
                .resolve_managed_mod_versions(&managed_mod, "1.20.4", &loaders)
                .await
                .unwrap_or_else(|error| panic!("{name} should resolve by slug fallback: {error}"));

            assert_eq!(versions.len(), 1);
            assert_eq!(versions[0].id, "declared-slug-version");
            let requests = requests.lock().expect("request log").clone();
            assert!(request_log_contains(
                &requests,
                "/v2/project/declared-project/version"
            ));
            assert!(request_log_contains(
                &requests,
                "/v2/project/declared-slug/version"
            ));
        }
    }

    #[tokio::test]
    async fn managed_install_does_not_fall_back_to_slug_on_rate_limit() {
        let (base_url, requests) =
            spawn_modrinth_identity_server(ProjectLookupResponse::RateLimited).await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
        let managed_mod = managed_mod("declared-project", "declared-slug");
        let loaders = vec!["fabric".to_string()];

        let error = manager
            .resolve_managed_mod_versions(&managed_mod, "1.20.4", &loaders)
            .await
            .expect_err("rate limit should not fall back to slug");

        assert!(matches!(
            error,
            InstallError::Modrinth(ModrinthError::RateLimited { .. })
        ));
        let requests = requests.lock().expect("request log").clone();
        assert!(request_log_contains(
            &requests,
            "/v2/project/declared-project/version"
        ));
        assert!(!request_log_contains(
            &requests,
            "/v2/project/declared-slug/version"
        ));
    }

    #[tokio::test]
    async fn ensure_installed_reuses_existing_verified_final_file() {
        let root = test_root("ensure-installed-reuse-verified-final");
        let existing = b"already-present-jar";
        fs::write(root.join("sodium.jar"), existing).expect("write existing final file");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_modrinth_server_with_sha512(Some(hex::encode(sha2::Sha512::digest(existing))))
                .await,
        )
        .expect("performance manager");
        let plan = CompositionPlan {
            composition_id: "core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![ManagedMod {
                artifact_id: "sodium".to_string(),
                project_id: "sodium".to_string(),
                slug: "sodium".to_string(),
                name: "Sodium".to_string(),
                condition: ModCondition::Always,
                version_range: String::new(),
                hardware_req: None,
                mutual_exclusions: Vec::new(),
            }],
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("reuse existing managed artifact");

        assert_eq!(state.installed_mods.len(), 1);
        assert!(state.installed_mods[0].integrity.sha512_verified);
        assert_eq!(
            fs::read(root.join("sodium.jar")).expect("read reused file"),
            existing
        );
        assert!(!root.join("sodium.jar.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_installed_removes_temp_and_leaves_no_final_on_sha512_mismatch() {
        let root = test_root("ensure-installed-sha512-mismatch");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_modrinth_server_with_sha512(Some("wrong-sha512".to_string())).await,
        )
        .expect("performance manager");
        let plan = CompositionPlan {
            composition_id: "core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![ManagedMod {
                artifact_id: "sodium".to_string(),
                project_id: "sodium".to_string(),
                slug: "sodium".to_string(),
                name: "Sodium".to_string(),
                condition: ModCondition::Always,
                version_range: String::new(),
                hardware_req: None,
                mutual_exclusions: Vec::new(),
            }],
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install should record failed managed artifact");

        assert_eq!(state.failure_count, 1);
        assert!(state.installed_mods.is_empty());
        assert!(!root.join("sodium.jar").exists());
        assert!(!root.join("sodium.jar.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

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
    fn remove_rejects_non_composition_owned_tracked_state_without_deleting_files() {
        let root = test_root("remove-rejects-user-owned-tracked-state");
        let manager = PerformanceManager::new().expect("performance manager");
        fs::create_dir_all(&root).expect("create mods dir");
        fs::write(root.join("user.jar"), b"user").expect("write user file");
        fs::write(
            root.join(".croopor-lock.json"),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "user.jar",
                    "ownership_class": "user_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
            }))
            .expect("serialize state"),
        )
        .expect("write invalid state");

        let error = manager
            .remove_managed(&root)
            .expect_err("invalid ownership should stop removal");

        assert!(matches!(
            error,
            InstallError::State(StateError::InvalidOwnership { .. })
        ));
        assert_eq!(fs::read(root.join("user.jar")).expect("read user"), b"user");
        assert!(root.join(".croopor-lock.json").is_file());
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
    fn remote_refresh_enabled_tracks_normalized_remote_url() {
        let root = test_root("remote-refresh-enabled");

        let unset = PerformanceManager::new_with_config_dir_and_remote_url(&root, None)
            .expect("performance manager");
        assert!(!unset.remote_refresh_enabled());

        let blank = PerformanceManager::new_with_config_dir_and_remote_url(
            &root,
            Some(" \t\n ".to_string()),
        )
        .expect("performance manager");
        assert!(!blank.remote_refresh_enabled());

        let configured = PerformanceManager::new_with_config_dir_and_remote_url(
            &root,
            Some(" https://rules.example.test/performance.json ".to_string()),
        )
        .expect("performance manager");
        assert!(configured.remote_refresh_enabled());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_uses_valid_cached_remote_rules_when_url_is_configured() {
        let root = test_root("startup-remote-cache");
        let builtin = builtin_manifest().expect("builtin manifest");
        let mut remote = builtin.clone();
        remote.generated_at = "2026-05-30T11:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        write_remote_rules_cache(&root, &remote, signature).expect("write remote cache");

        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
            Some(public_key),
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
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-05-30T12:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
            Some(public_key.clone()),
        )
        .expect("performance manager");

        manager
            .accept_remote_manifest(
                &root,
                RemoteRulesCandidate {
                    manifest: remote.clone(),
                    signature,
                },
            )
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
            &RemoteRulesVerifier::from_public_key_hex(Some(public_key)),
        );
        assert_eq!(reloaded.rule_source, RuleSource::Remote);
        assert_eq!(reloaded.manifest.generated_at, remote.generated_at);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_remote_refresh_keeps_previous_rules_and_exposes_warning() {
        let root = test_root("reject-remote-refresh");
        let mut invalid = builtin_manifest().expect("builtin manifest");
        invalid.schema_version = 99;
        let (public_key, signature) = signed_metadata(&invalid);
        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
            Some(public_key),
        )
        .expect("performance manager");
        let before = manager.rules_status();

        let error = manager
            .accept_remote_manifest(
                &root,
                RemoteRulesCandidate {
                    manifest: invalid,
                    signature,
                },
            )
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

    #[test]
    fn remote_refresh_without_public_key_keeps_builtin_and_exposes_warning() {
        let root = test_root("remote-refresh-missing-public-key");
        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some("https://rules.example.test/performance.json".to_string()),
            None,
        )
        .expect("performance manager");

        assert!(manager.remote_refresh_enabled());
        let status = manager.rules_status();
        assert_eq!(status.rule_source, RuleSource::BuiltIn);
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("public key is not configured"))
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
            ownership_class: OwnershipClass::CompositionManaged,
            source: modrinth_source(),
            integrity: ManagedArtifactIntegrity {
                sha512: String::new(),
                sha512_verified: false,
            },
        }
    }

    fn managed_mod(project_id: &str, slug: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: "declared-artifact".to_string(),
            project_id: project_id.to_string(),
            slug: slug.to_string(),
            name: "Declared Artifact".to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }

    fn signed_metadata(manifest: &crate::types::Manifest) -> (String, RulesSignatureMetadata) {
        let signing_key = SigningKey::from_bytes(&[11_u8; 32]);
        let payload = crate::signature::canonical_manifest_payload(manifest).expect("payload");
        let signature = signing_key.sign(&payload);
        (
            hex::encode(signing_key.verifying_key().to_bytes()),
            RulesSignatureMetadata {
                signature: hex::encode(signature.to_bytes()),
                key_id: Some("install-test-key".to_string()),
            },
        )
    }

    async fn spawn_modrinth_server(include_sha512: bool) -> String {
        let sha512 = if include_sha512 {
            Some(hex::encode(sha2::Sha512::digest(b"managed-jar")))
        } else {
            None
        };
        spawn_modrinth_server_with_sha512(sha512).await
    }

    #[derive(Clone, Copy)]
    enum ProjectLookupResponse {
        Version,
        NotFound,
        Empty,
        RateLimited,
    }

    async fn spawn_modrinth_identity_server(
        project_response: ProjectLookupResponse,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind modrinth identity test server");
        let addr = listener.local_addr().expect("modrinth identity test addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let request_log = Arc::clone(&request_log);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut buf).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 8192 {
                            return;
                        }
                    }

                    let request = String::from_utf8_lossy(&request);
                    let first_line = request.lines().next().unwrap_or_default().to_string();
                    request_log
                        .lock()
                        .expect("record request")
                        .push(first_line.clone());

                    let (status, content_type, extra_headers, body) =
                        if first_line.contains("/v2/project/declared-project/version") {
                            match project_response {
                                ProjectLookupResponse::Version => (
                                    "200 OK",
                                    "application/json",
                                    String::new(),
                                    version_response_body(&addr, "declared-project"),
                                ),
                                ProjectLookupResponse::NotFound => (
                                    "404 Not Found",
                                    "text/plain",
                                    String::new(),
                                    b"not found".to_vec(),
                                ),
                                ProjectLookupResponse::Empty => {
                                    ("200 OK", "application/json", String::new(), b"[]".to_vec())
                                }
                                ProjectLookupResponse::RateLimited => (
                                    "429 Too Many Requests",
                                    "text/plain",
                                    "X-Ratelimit-Reset: 13\r\n".to_string(),
                                    b"try later".to_vec(),
                                ),
                            }
                        } else if first_line.contains("/v2/project/declared-slug/version") {
                            (
                                "200 OK",
                                "application/json",
                                String::new(),
                                version_response_body(&addr, "declared-slug"),
                            )
                        } else {
                            (
                                "404 Not Found",
                                "text/plain",
                                String::new(),
                                b"not found".to_vec(),
                            )
                        };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n",
                        body.len()
                    );
                    if stream.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn version_response_body(addr: &std::net::SocketAddr, project_ref: &str) -> Vec<u8> {
        let file_url = format!("http://{addr}/files/{project_ref}.jar");
        format!(
            r#"[{{"id":"{project_ref}-version","game_versions":["1.20.4"],"loaders":["fabric"],"featured":true,"date_published":"2026-05-30T00:00:00Z","files":[{{"url":"{file_url}","filename":"{project_ref}.jar","primary":true,"hashes":{{}}}}]}}]"#
        )
        .into_bytes()
    }

    fn request_log_contains(requests: &[String], needle: &str) -> bool {
        requests.iter().any(|request| request.contains(needle))
    }

    async fn spawn_modrinth_server_with_sha512(sha512: Option<String>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind modrinth test server");
        let addr = listener.local_addr().expect("modrinth test addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let sha512 = sha512.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut buf).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 8192 {
                            return;
                        }
                    }

                    let request = String::from_utf8_lossy(&request);
                    let first_line = request.lines().next().unwrap_or_default();
                    let file_url = format!("http://{addr}/files/sodium.jar");
                    let hashes = if let Some(sha512) = sha512.as_ref() {
                        format!(r#""sha512":"{sha512}""#)
                    } else {
                        String::new()
                    };
                    let (status, content_type, body) = if first_line
                        .contains("/v2/project/sodium/version")
                    {
                        let body = format!(
                            r#"[{{"id":"version-a","game_versions":["1.20.4"],"loaders":["fabric"],"featured":true,"date_published":"2026-05-30T00:00:00Z","files":[{{"url":"{file_url}","filename":"sodium.jar","primary":true,"hashes":{{{hashes}}}}}]}}]"#
                        );
                        ("200 OK", "application/json", body.into_bytes())
                    } else if first_line.contains("/files/sodium.jar") {
                        (
                            "200 OK",
                            "application/octet-stream",
                            b"managed-jar".to_vec(),
                        )
                    } else {
                        ("404 Not Found", "text/plain", b"not found".to_vec())
                    };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        format!("http://{addr}")
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
