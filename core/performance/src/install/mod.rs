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
    load_rollback_snapshot_async, load_rollback_snapshot_by_id, load_state, managed_artifact_path,
    remove_state, restore_rollback_snapshot, restore_rollback_snapshot_async,
    save_rollback_snapshot, save_rollback_snapshot_async, save_state,
};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::{
    CompositionPlan, CompositionState, CompositionTier, EmergencyDisable, EmergencyDisableTarget,
    InstalledMod, ManagedArtifactIntegrity, ManagedArtifactProvider, ManagedArtifactSource,
    ManagedMod, Manifest, OwnershipClass, PerformanceMode, ResolutionRequest, VersionFamily,
};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tracing::warn;

pub const PERFORMANCE_RULES_URL_ENV: &str = "CROOPOR_PERFORMANCE_RULES_URL";
const REMOTE_RULES_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_RULES_MAX_BYTES: usize = 1024 * 1024;
const MIN_USEFUL_MANAGED_INSTALLS: usize = 2;
const MAX_INSTALL_FALLBACK_ATTEMPTS: usize = 4;
const MANAGED_ARTIFACT_INSTALL_CONCURRENCY: usize = 4;
const MANAGED_ARTIFACT_INSTALL_FAILURE: &str = "managed artifact install failed";

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
    #[error("managed artifact target already exists: {0}")]
    ManagedArtifactTargetExists(String),
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
        let active = active_rules_read(&self.active);
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

    pub fn manifest(&self) -> crate::types::Manifest {
        active_rules_read(&self.active).manifest.clone()
    }

    pub fn rules_status(&self) -> crate::status::PerformanceRulesStatus {
        let active = active_rules_read(&self.active);
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
                    self.record_refresh_warning(remote_rules_refresh_warning("failed", &error));
                    Ok(self.rules_status())
                }
            },
            Err(error) => {
                self.record_refresh_warning(remote_rules_refresh_warning("rejected", &error));
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
        let mut active = active_rules_write(&self.active);
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
        let mut active = active_rules_write(&self.active);
        active.rules_cache.warning = Some(bounded_warning(warning));
    }

    async fn install_mod(
        &self,
        managed_mod: &crate::types::ManagedMod,
        game_version: &str,
        loader: &str,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
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
        let was_previously_tracked = state_tracks_filename(previous_state, &filename);

        if tokio::fs::try_exists(&final_path).await? {
            if !expected_sha.trim().is_empty()
                && let Ok(true) = file_matches_sha512(&final_path, &expected_sha, file.size).await
            {
                return Ok(InstalledMod {
                    project_id: managed_mod.project_id.clone(),
                    version_id: version.id,
                    filename,
                    ownership_class: OwnershipClass::CompositionManaged,
                    source: modrinth_source(),
                    integrity: verified_sha512_integrity(expected_sha),
                });
            }
            if !was_previously_tracked {
                return Err(InstallError::ManagedArtifactTargetExists(filename));
            }
        }

        let temp_path = managed_artifact_temp_path(&final_path, managed_mod);
        self.modrinth
            .download_file_to_path(&file.url, &expected_sha, file.size, &temp_path)
            .await?;
        if tokio::fs::try_exists(&final_path).await? && !was_previously_tracked {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(InstallError::ManagedArtifactTargetExists(filename));
        }
        promote_file_async(&temp_path, &final_path, &filename, was_previously_tracked).await?;

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

    async fn attempt_install_plan(
        &self,
        plan: &CompositionPlan,
        game_version: &str,
        instance_mods_dir: &Path,
        previous_state: Option<&CompositionState>,
    ) -> CompositionState {
        let mut state = CompositionState {
            composition_id: plan.composition_id.clone(),
            tier: plan.tier,
            installed_mods: Vec::with_capacity(plan.mods.len()),
            installed_at: Utc::now().to_rfc3339(),
            failure_count: 0,
            last_failure: String::new(),
        };

        let loader = plan.loader.clone();
        let game_version = game_version.to_string();
        let instance_mods_dir = instance_mods_dir.to_path_buf();
        let previous_state = previous_state.cloned();
        let mut installs = stream::iter(plan.mods.iter().cloned().map(|managed_mod| {
            let loader = loader.clone();
            let game_version = game_version.clone();
            let instance_mods_dir = instance_mods_dir.clone();
            let previous_state = previous_state.clone();
            async move {
                let project_id = managed_mod.project_id.clone();
                let result = self
                    .install_mod(
                        &managed_mod,
                        &game_version,
                        &loader,
                        &instance_mods_dir,
                        previous_state.as_ref(),
                    )
                    .await;
                (project_id, result)
            }
        }))
        .buffer_unordered(managed_artifact_install_concurrency(plan.mods.len()));

        while let Some((project_id, result)) = installs.next().await {
            match result {
                Ok(installed) => state.installed_mods.push(installed),
                Err(error) => {
                    state.failure_count += 1;
                    state.last_failure = managed_artifact_failure_evidence();
                    warn!("performance install failed for {project_id}: {error}");
                }
            }
        }

        state
            .installed_mods
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        state
    }

    fn install_attempt_plans(&self, plan: &CompositionPlan) -> Vec<CompositionPlan> {
        let active = active_rules_read(&self.active);
        let manifest = &active.manifest;
        let mut plans = vec![plan.clone()];
        let mut seen = std::collections::HashSet::new();
        if !plan.composition_id.is_empty() {
            seen.insert(plan.composition_id.clone());
        }

        for (chain_index, fallback_id) in plan
            .fallback_chain
            .iter()
            .enumerate()
            .filter(|(_, fallback_id)| !fallback_id.trim().is_empty())
            .take(MAX_INSTALL_FALLBACK_ATTEMPTS.saturating_sub(1))
        {
            if !seen.insert(fallback_id.clone()) {
                warn!("performance fallback cycle ignored at {}", fallback_id);
                continue;
            }

            let Some(definition) = manifest
                .compositions
                .iter()
                .find(|definition| definition.id == *fallback_id)
            else {
                warn!(
                    "performance fallback composition {} is missing",
                    fallback_id
                );
                continue;
            };

            if !definition.families.contains(&plan.family)
                || !definition
                    .loaders
                    .iter()
                    .any(|loader| loader.eq_ignore_ascii_case(&plan.loader))
            {
                warn!(
                    "performance fallback composition {} does not apply to {:?}/{}",
                    fallback_id, plan.family, plan.loader
                );
                continue;
            }

            let mods = definition
                .mods
                .iter()
                .filter(|managed_mod| {
                    active_artifact_disable(
                        manifest,
                        managed_mod,
                        plan.family,
                        &plan.loader,
                        definition.tier,
                    )
                    .is_none()
                })
                .cloned()
                .collect();

            plans.push(CompositionPlan {
                composition_id: definition.id.clone(),
                family: plan.family,
                loader: plan.loader.clone(),
                mode: plan.mode,
                tier: definition.tier,
                mods,
                jvm_preset: definition.jvm_preset.clone(),
                fallback_chain: plan
                    .fallback_chain
                    .iter()
                    .skip(chain_index + 1)
                    .cloned()
                    .collect(),
                warnings: plan.warnings.clone(),
                fallback_reason: format!(
                    "install fallback from {} after severe managed install failure",
                    plan.composition_id
                ),
            });
        }

        plans
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

fn active_rules_read(active: &RwLock<ActiveRules>) -> RwLockReadGuard<'_, ActiveRules> {
    active.read().unwrap_or_else(|poisoned| {
        warn!("performance rules lock poisoned during read; recovering active rules");
        poisoned.into_inner()
    })
}

fn active_rules_write(active: &RwLock<ActiveRules>) -> RwLockWriteGuard<'_, ActiveRules> {
    active.write().unwrap_or_else(|poisoned| {
        warn!("performance rules lock poisoned during write; recovering active rules");
        poisoned.into_inner()
    })
}

fn managed_artifact_install_concurrency(mod_count: usize) -> usize {
    mod_count.max(1).min(MANAGED_ARTIFACT_INSTALL_CONCURRENCY)
}

fn managed_artifact_temp_path(final_path: &Path, managed_mod: &ManagedMod) -> PathBuf {
    let suffix = safe_temp_suffix(&managed_mod.project_id);
    PathBuf::from(format!("{}.{}.tmp", final_path.display(), suffix))
}

fn safe_temp_suffix(value: &str) -> String {
    let suffix: String = value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
        .take(48)
        .map(char::from)
        .collect();
    if suffix.is_empty() {
        "managed".to_string()
    } else {
        suffix
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

fn severe_install_failure(plan: &CompositionPlan, state: &CompositionState) -> bool {
    !plan.mods.is_empty()
        && state.failure_count > 0
        && state.installed_mods.len() < MIN_USEFUL_MANAGED_INSTALLS
}

fn active_artifact_disable<'a>(
    manifest: &'a Manifest,
    managed_mod: &ManagedMod,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> Option<&'a EmergencyDisable> {
    manifest.emergency_disables.iter().find(|disable| {
        disable.target == EmergencyDisableTarget::Artifact
            && artifact_target_matches(manifest, disable, managed_mod)
            && disable_applies(disable, family, loader, tier)
    })
}

fn artifact_target_matches(
    manifest: &Manifest,
    disable: &EmergencyDisable,
    managed_mod: &ManagedMod,
) -> bool {
    let Some(artifact) = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.id.eq_ignore_ascii_case(&managed_mod.artifact_id))
    else {
        return false;
    };
    disable.target_id.eq_ignore_ascii_case(&artifact.id)
        || disable
            .target_id
            .eq_ignore_ascii_case(&artifact.source.project_id)
        || disable
            .target_id
            .eq_ignore_ascii_case(&artifact.source.slug)
}

fn disable_applies(
    disable: &EmergencyDisable,
    family: VersionFamily,
    loader: &str,
    tier: CompositionTier,
) -> bool {
    (disable.families.is_empty() || disable.families.contains(&family))
        && (disable.loaders.is_empty()
            || disable
                .loaders
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(loader)))
        && (disable.tiers.is_empty() || disable.tiers.contains(&tier))
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

async fn file_matches_sha512(
    path: &Path,
    expected_sha512: &str,
    expected_size: Option<u64>,
) -> Result<bool, std::io::Error> {
    if let Some(expected_size) = expected_size
        && tokio::fs::metadata(path).await?.len() != expected_size
    {
        return Ok(false);
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha512::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_sha512))
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

fn state_tracks_filename(state: Option<&CompositionState>, filename: &str) -> bool {
    state.is_some_and(|state| {
        state
            .installed_mods
            .iter()
            .any(|installed| installed.filename == filename)
    })
}

fn managed_artifact_failure_evidence() -> String {
    MANAGED_ARTIFACT_INSTALL_FAILURE.to_string()
}

async fn promote_file_async(
    temp_path: &Path,
    final_path: &Path,
    filename: &str,
    allow_overwrite: bool,
) -> Result<(), InstallError> {
    if allow_overwrite {
        return promote_file_with_overwrite_async(temp_path, final_path).await;
    }

    match tokio::fs::hard_link(temp_path, final_path).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::ManagedArtifactTargetExists(
                filename.to_string(),
            ))
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::Io(error))
        }
    }
}

#[cfg(test)]
fn promote_file_with_overwrite(temp_path: &Path, final_path: &Path) -> Result<(), InstallError> {
    let first_error = match fs::rename(temp_path, final_path) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fs::symlink_metadata(temp_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallError::Io(first_error));
        }
        Err(error) => return Err(InstallError::Io(error)),
    }

    let final_metadata = match fs::symlink_metadata(final_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            let _ = fs::remove_file(temp_path);
            return Err(InstallError::Io(error));
        }
    };

    let Some(final_metadata) = final_metadata else {
        return match fs::rename(temp_path, final_path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                Err(InstallError::Io(error))
            }
        };
    };

    if final_metadata.is_dir() && !final_metadata.file_type().is_symlink() {
        let _ = fs::remove_file(temp_path);
        return Err(InstallError::Io(first_error));
    }

    let backup_path = managed_artifact_replace_backup_path(final_path);
    fs::rename(final_path, &backup_path).map_err(|error| {
        let _ = fs::remove_file(temp_path);
        InstallError::Io(error)
    })?;

    match fs::rename(temp_path, final_path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup_path);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup_path, final_path);
            let _ = fs::remove_file(temp_path);
            Err(InstallError::Io(error))
        }
    }
}

async fn promote_file_with_overwrite_async(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), InstallError> {
    let first_error = match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match tokio::fs::symlink_metadata(temp_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InstallError::Io(first_error));
        }
        Err(error) => return Err(InstallError::Io(error)),
    }

    let final_metadata = match tokio::fs::symlink_metadata(final_path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            let _ = tokio::fs::remove_file(temp_path).await;
            return Err(InstallError::Io(error));
        }
    };

    let Some(final_metadata) = final_metadata else {
        return match tokio::fs::rename(temp_path, final_path).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = tokio::fs::remove_file(temp_path).await;
                Err(InstallError::Io(error))
            }
        };
    };

    if final_metadata.is_dir() && !final_metadata.file_type().is_symlink() {
        let _ = tokio::fs::remove_file(temp_path).await;
        return Err(InstallError::Io(first_error));
    }

    let backup_path = managed_artifact_replace_backup_path(final_path);
    if let Err(error) = tokio::fs::rename(final_path, &backup_path).await {
        let _ = tokio::fs::remove_file(temp_path).await;
        return Err(InstallError::Io(error));
    }

    match tokio::fs::rename(temp_path, final_path).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&backup_path).await;
            Ok(())
        }
        Err(error) => {
            let _ = tokio::fs::rename(&backup_path, final_path).await;
            let _ = tokio::fs::remove_file(temp_path).await;
            Err(InstallError::Io(error))
        }
    }
}

fn managed_artifact_replace_backup_path(final_path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    PathBuf::from(format!(
        "{}.replace-{}-{nanos:x}.tmp",
        final_path.display(),
        std::process::id()
    ))
}

fn configured_remote_rules_url() -> Option<String> {
    normalize_remote_rules_url(std::env::var(PERFORMANCE_RULES_URL_ENV).ok())
}

fn normalize_remote_rules_url(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn remote_rules_refresh_warning(action: &str, error: &RulesRefreshError) -> String {
    match error {
        RulesRefreshError::Request(_) => {
            format!("Remote rules refresh {action}: request failed; using previously active rules.")
        }
        RulesRefreshError::Cache(_) => format!(
            "Remote rules refresh {action}: remote rules cache could not be persisted; using previously active rules."
        ),
        RulesRefreshError::Unconfigured
        | RulesRefreshError::HttpStatus(_)
        | RulesRefreshError::ResponseTooLarge
        | RulesRefreshError::Parse(_)
        | RulesRefreshError::Validation(_)
        | RulesRefreshError::Signature(_) => format!("Remote rules refresh {action}: {error}"),
    }
}

fn rules_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("croopor/0.3.1 performance-rules")
        .timeout(REMOTE_RULES_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{BundleHealth, derive_health};
    use crate::state::{load_state, save_state};
    use crate::types::{
        CompositionDef, CompositionTier, EmergencyDisable, EmergencyDisableTarget, ManagedMod,
        ModCondition, VersionFamily,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CURRENT_FAMILY_F_REPRESENTATIVE: &str = "1.21.1";

    #[test]
    fn managed_artifact_install_concurrency_is_bounded() {
        assert_eq!(managed_artifact_install_concurrency(0), 1);
        assert_eq!(managed_artifact_install_concurrency(1), 1);
        assert_eq!(managed_artifact_install_concurrency(3), 3);
        assert_eq!(managed_artifact_install_concurrency(12), 4);
    }

    #[test]
    fn active_rules_helpers_recover_poisoned_read_and_write() {
        let manager = PerformanceManager::new().expect("performance manager");
        let active = Arc::clone(&manager.active);

        let poison = std::thread::spawn(move || {
            let mut active = active.write().expect("active rules lock");
            active.rules_cache.warning = Some("poisoned while updating rules".to_string());
            panic!("poison active rules lock");
        })
        .join();
        assert!(poison.is_err());

        {
            let active = active_rules_read(&manager.active);
            assert_eq!(
                active.rules_cache.warning.as_deref(),
                Some("poisoned while updating rules")
            );
        }

        {
            let mut active = active_rules_write(&manager.active);
            active.rules_cache.warning = Some("recovered write".to_string());
        }

        let active = active_rules_read(&manager.active);
        assert_eq!(
            active.rules_cache.warning.as_deref(),
            Some("recovered write")
        );
    }

    #[test]
    fn managed_artifact_temp_path_uses_safe_project_suffix() {
        let managed_mod = ManagedMod {
            artifact_id: "artifact".to_string(),
            project_id: "../project id/with/slash".to_string(),
            slug: "slug".to_string(),
            name: "Managed Mod".to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        };

        let temp_path = managed_artifact_temp_path(Path::new("/tmp/mods/mod.jar"), &managed_mod);

        assert_eq!(
            temp_path,
            PathBuf::from("/tmp/mods/mod.jar.projectidwithslash.tmp")
        );
    }

    #[test]
    fn promote_file_with_overwrite_replaces_existing_file() {
        let root = test_root("promote-overwrite-replaces-file");
        let temp_path = root.join("sodium.jar.tmp");
        let final_path = root.join("sodium.jar");
        fs::write(&temp_path, b"fresh").expect("write temp artifact");
        fs::write(&final_path, b"existing").expect("write existing artifact");

        promote_file_with_overwrite(&temp_path, &final_path).expect("promote replacement");

        assert_eq!(
            fs::read(&final_path).expect("read promoted artifact"),
            b"fresh"
        );
        assert!(!temp_path.exists());
        assert_no_replace_backups(&root);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn promote_file_with_overwrite_preserves_existing_file_when_temp_is_missing() {
        let root = test_root("promote-overwrite-missing-temp");
        let temp_path = root.join("sodium.jar.tmp");
        let final_path = root.join("sodium.jar");
        fs::write(&final_path, b"existing").expect("write existing artifact");

        promote_file_with_overwrite(&temp_path, &final_path).expect_err("missing temp should fail");

        assert_eq!(
            fs::read(&final_path).expect("read existing artifact"),
            b"existing"
        );
        assert!(!temp_path.exists());
        assert_no_replace_backups(&root);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn promote_file_with_overwrite_preserves_directory_destination_and_removes_temp() {
        let root = test_root("promote-overwrite-directory");
        let temp_path = root.join("sodium.jar.tmp");
        let final_path = root.join("sodium.jar");
        fs::write(&temp_path, b"fresh").expect("write temp artifact");
        fs::create_dir(&final_path).expect("create directory destination");

        promote_file_with_overwrite(&temp_path, &final_path)
            .expect_err("directory destination should fail");

        assert!(final_path.is_dir());
        assert!(!temp_path.exists());
        assert_no_replace_backups(&root);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_promote_file_with_overwrite_preserves_directory_destination_and_removes_temp() {
        let root = test_root("async-promote-overwrite-directory");
        let temp_path = root.join("sodium.jar.tmp");
        let final_path = root.join("sodium.jar");
        fs::write(&temp_path, b"fresh").expect("write temp artifact");
        fs::create_dir(&final_path).expect("create directory destination");

        promote_file_with_overwrite_async(&temp_path, &final_path)
            .await
            .expect_err("directory destination should fail");

        assert!(final_path.is_dir());
        assert!(!temp_path.exists());
        assert_no_replace_backups(&root);

        let _ = fs::remove_dir_all(root);
    }

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
    async fn representative_modern_fabric_plans_install_without_composition_fallback() {
        let (base_url, requests) = spawn_representative_modrinth_server(
            &["1.16.5", "1.20.1", CURRENT_FAMILY_F_REPRESENTATIVE],
            &["fabric"],
        )
        .await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");

        for (game_version, expected_family, expected_composition) in [
            ("1.16.5", VersionFamily::E, "family-e-fabric-extended"),
            ("1.20.1", VersionFamily::E, "family-e-fabric-extended"),
            (
                CURRENT_FAMILY_F_REPRESENTATIVE,
                VersionFamily::F,
                "family-f-fabric-extended",
            ),
        ] {
            let root = test_root(&format!(
                "representative-modern-fabric-{}",
                game_version.replace('.', "-")
            ));
            fs::write(root.join("user-owned.jar"), b"user-owned").expect("write user-owned mod");
            let plan = manager.get_plan(ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: crate::types::HardwareProfile::default(),
                installed_mods: vec!["user-owned".to_string()],
            });
            let planned_projects = plan
                .mods
                .iter()
                .map(|managed_mod| managed_mod.project_id.clone())
                .collect::<Vec<_>>();

            assert_eq!(plan.composition_id, expected_composition, "{game_version}");
            assert_eq!(plan.family, expected_family, "{game_version}");
            assert_eq!(plan.tier, CompositionTier::Extended, "{game_version}");
            assert!(!plan.mods.is_empty(), "{game_version}");
            assert!(
                plan.fallback_chain
                    .iter()
                    .any(|fallback| fallback.contains("core")),
                "{game_version}"
            );

            let state = manager
                .ensure_installed(&plan, game_version, &root)
                .await
                .unwrap_or_else(|error| panic!("{game_version} representative install: {error}"));

            assert_eq!(state.composition_id, expected_composition, "{game_version}");
            assert_eq!(state.tier, CompositionTier::Extended, "{game_version}");
            assert_eq!(state.failure_count, 0, "{game_version}");
            assert_eq!(
                state.installed_mods.len(),
                planned_projects.len(),
                "{game_version}"
            );
            assert_eq!(
                state
                    .installed_mods
                    .iter()
                    .map(|installed| installed.project_id.clone())
                    .collect::<Vec<_>>(),
                sorted_projects(planned_projects),
                "{game_version}"
            );

            let loaded = load_state(&root)
                .expect("load representative state")
                .expect("representative state should be saved");
            assert_eq!(
                loaded.composition_id, expected_composition,
                "{game_version}"
            );
            assert_eq!(loaded.failure_count, 0, "{game_version}");
            assert_eq!(
                loaded.installed_mods.len(),
                state.installed_mods.len(),
                "{game_version}"
            );
            assert!(
                loaded
                    .installed_mods
                    .iter()
                    .all(|installed| installed.ownership_class
                        == OwnershipClass::CompositionManaged
                        && installed.source.provider == ManagedArtifactProvider::Modrinth
                        && installed.integrity.sha512_verified),
                "{game_version}"
            );
            assert_eq!(
                fs::read(root.join("user-owned.jar")).expect("read user-owned mod"),
                b"user-owned",
                "{game_version}"
            );
            assert!(
                loaded
                    .installed_mods
                    .iter()
                    .all(|installed| installed.project_id != "user-owned"),
                "{game_version}"
            );
            for installed in &loaded.installed_mods {
                assert!(root.join(&installed.filename).is_file(), "{game_version}");
            }

            let _ = fs::remove_dir_all(root);
        }

        let requests = requests.lock().expect("request log").clone();
        for project in [
            "sodium",
            "lithium",
            "ferrite-core",
            "immediatelyfast",
            "dynamic-fps",
            "modernfix",
        ] {
            assert!(
                request_log_contains(&requests, &format!("/v2/project/{project}/version")),
                "{project}"
            );
            assert!(
                request_log_contains(&requests, &format!("/files/{project}.jar")),
                "{project}"
            );
        }
    }

    #[tokio::test]
    async fn family_c_forge_core_installs_with_mocked_modrinth_artifacts() {
        let (base_url, requests) =
            spawn_representative_modrinth_server(&["1.12.2"], &["forge"]).await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
        let root = test_root("family-c-forge-core");
        fs::write(root.join("user-owned.jar"), b"user-owned").expect("write user-owned mod");

        let plan = manager.get_plan(ResolutionRequest {
            game_version: "1.12.2".to_string(),
            loader: "forge".to_string(),
            mode: PerformanceMode::Managed,
            hardware: crate::types::HardwareProfile::default(),
            installed_mods: vec!["user-owned".to_string()],
        });
        let planned_projects = plan
            .mods
            .iter()
            .map(|managed_mod| managed_mod.project_id.clone())
            .collect::<Vec<_>>();

        assert_eq!(plan.composition_id, "family-c-forge-core");
        assert_eq!(plan.tier, CompositionTier::Core);
        assert_eq!(
            plan.fallback_chain,
            vec!["family-c-vanilla-enhanced".to_string()]
        );
        assert_eq!(count_plan_mods_with_slug(&plan.mods, "foamfix"), 1);
        assert_eq!(count_plan_mods_with_slug(&plan.mods, "ai-improvements"), 1);
        assert_eq!(count_plan_mods_with_slug(&plan.mods, "clumps"), 1);

        let state = manager
            .ensure_installed(&plan, "1.12.2", &root)
            .await
            .expect("install family c forge core artifacts");

        assert_eq!(state.composition_id, "family-c-forge-core");
        assert_eq!(state.tier, CompositionTier::Core);
        assert_eq!(state.failure_count, 0);
        assert_eq!(state.installed_mods.len(), planned_projects.len());
        assert_eq!(
            state
                .installed_mods
                .iter()
                .map(|installed| installed.project_id.clone())
                .collect::<Vec<_>>(),
            sorted_projects(planned_projects)
        );

        let loaded = load_state(&root)
            .expect("load family c forge state")
            .expect("family c forge state should be saved");
        assert_eq!(loaded.composition_id, "family-c-forge-core");
        assert_eq!(loaded.tier, CompositionTier::Core);
        assert_eq!(loaded.failure_count, 0);
        assert!(loaded.installed_mods.iter().all(|installed| {
            installed.ownership_class == OwnershipClass::CompositionManaged
                && installed.source.provider == ManagedArtifactProvider::Modrinth
                && installed.integrity.sha512_verified
                && !installed.integrity.sha512.is_empty()
        }));
        assert_eq!(
            fs::read(root.join("user-owned.jar")).expect("read user-owned mod"),
            b"user-owned"
        );

        for installed in &loaded.installed_mods {
            assert!(root.join(&installed.filename).is_file());
        }

        let requests = requests.lock().expect("request log").clone();
        for project_ref in ["jupr7Bf5", "DSVgwcji", "clumps"] {
            assert!(
                request_log_contains(&requests, &format!("/v2/project/{project_ref}/version")),
                "{project_ref}"
            );
            assert!(
                request_log_contains(&requests, &format!("/files/{project_ref}.jar")),
                "{project_ref}"
            );
        }
        assert!(!request_log_contains(
            &requests,
            "/v2/project/family-c-vanilla-enhanced/version"
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_installed_reuses_existing_verified_final_file() {
        let root = test_root("ensure-installed-reuse-verified-final");
        let existing = b"already-present-jar";
        fs::write(root.join("sodium.jar"), existing).expect("write existing final file");
        let (base_url, requests) = spawn_modrinth_server_with_sha512_size_and_requests(
            Some(hex::encode(sha2::Sha512::digest(existing))),
            Some(existing.len() as u64),
        )
        .await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
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
        let requests = requests.lock().expect("request log").clone();
        assert!(request_log_contains(
            &requests,
            "/v2/project/sodium/version"
        ));
        assert!(!request_log_contains(&requests, "/files/sodium.jar"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_installed_rejects_existing_final_file_with_wrong_size() {
        let root = test_root("ensure-installed-reject-wrong-size-final");
        let existing = b"already-present-jar";
        fs::write(root.join("sodium.jar"), existing).expect("write existing final file");
        let (base_url, requests) = spawn_modrinth_server_with_sha512_size_and_requests(
            Some(hex::encode(sha2::Sha512::digest(existing))),
            Some(existing.len() as u64 + 1),
        )
        .await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
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
        assert_eq!(
            fs::read(root.join("sodium.jar")).expect("read protected file"),
            existing
        );
        assert!(!root.join("sodium.jar.tmp").exists());
        let requests = requests.lock().expect("request log").clone();
        assert!(request_log_contains(
            &requests,
            "/v2/project/sodium/version"
        ));
        assert!(!request_log_contains(&requests, "/files/sodium.jar"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_artifact_reuse_future_stays_small_enough_for_tokio_workers() {
        assert!(
            std::mem::size_of_val(&file_matches_sha512(
                Path::new("/tmp/croopor-test/sodium.jar"),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(8),
            )) < 4096,
            "managed artifact reuse hash future should not embed the hash buffer on the task stack"
        );
    }

    #[tokio::test]
    async fn ensure_installed_does_not_overwrite_existing_mismatched_final_file() {
        let root = test_root("ensure-installed-preserve-existing-final");
        let existing = b"user-created-sodium";
        fs::write(root.join("sodium.jar"), existing).expect("write existing user file");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_modrinth_server_with_sha512(Some(hex::encode(sha2::Sha512::digest(
                b"managed-jar",
            ))))
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
            .expect("install should record failed managed artifact");

        assert_eq!(state.failure_count, 1);
        assert_eq!(state.last_failure, MANAGED_ARTIFACT_INSTALL_FAILURE);
        assert!(state.installed_mods.is_empty());
        assert_eq!(
            fs::read(root.join("sodium.jar")).expect("read existing user file"),
            existing
        );
        assert!(!root.join("sodium.jar.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_managed_install_stores_product_safe_failure_evidence() {
        let root = test_root("safe-failure-evidence");
        let (base_url, leaked_details) = spawn_leaky_download_failure_server().await;
        let manager =
            PerformanceManager::new_with_modrinth_base_url(base_url).expect("performance manager");
        let plan = CompositionPlan {
            composition_id: "core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![managed_mod("sodium", "sodium")],
            jvm_preset: String::new(),
            fallback_chain: Vec::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install should record product-safe failure evidence");
        let loaded = load_state(&root)
            .expect("load state")
            .expect("failed state should be saved");
        let (health, warnings) = derive_health(Some(&loaded), Some(&plan), &root);
        let warning_text = warnings.join("\n");

        assert_eq!(state.failure_count, 1);
        assert_eq!(state.last_failure, MANAGED_ARTIFACT_INSTALL_FAILURE);
        assert_eq!(loaded.failure_count, 1);
        assert_eq!(loaded.last_failure, MANAGED_ARTIFACT_INSTALL_FAILURE);
        assert_eq!(health, BundleHealth::Degraded);
        assert_eq!(
            warnings,
            vec!["1 managed mod install failure(s): managed artifact install failed"]
        );
        for detail in leaked_details {
            assert!(!state.last_failure.contains(&detail), "{detail}");
            assert!(!loaded.last_failure.contains(&detail), "{detail}");
            assert!(!warning_text.contains(&detail), "{detail}");
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ensure_installed_replaces_previously_tracked_mismatched_final_file() {
        let root = test_root("ensure-installed-replace-tracked-final");
        fs::write(root.join("sodium.jar"), b"old-managed-sodium")
            .expect("write previous managed file");
        save_state(
            &root,
            &test_state("core", vec![test_mod("sodium", "sodium.jar")]),
        )
        .expect("save previous state");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_modrinth_server_with_sha512(Some(hex::encode(sha2::Sha512::digest(
                b"managed-jar",
            ))))
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
            .expect("replace previous tracked managed artifact");

        assert_eq!(state.failure_count, 0);
        assert_eq!(state.installed_mods.len(), 1);
        assert!(state.installed_mods[0].integrity.sha512_verified);
        assert_eq!(
            fs::read(root.join("sodium.jar")).expect("read replaced file"),
            b"managed-jar"
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

    #[tokio::test]
    async fn ensure_installed_passes_modrinth_file_size_to_download() {
        let root = test_root("ensure-installed-file-size");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_modrinth_server_with_sha512_and_size(None, Some(4)).await,
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

    #[tokio::test]
    async fn severe_extended_failure_installs_and_saves_core_fallback() {
        let root = test_root("install-severe-fallback-core");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_selective_modrinth_server(&["sodium", "lithium"]).await,
        )
        .expect("performance manager");
        set_test_compositions(
            &manager,
            vec![composition_def(
                "test-core",
                CompositionTier::Core,
                vec![
                    managed_mod("sodium", "sodium"),
                    managed_mod("lithium", "lithium"),
                ],
            )],
        );
        let plan = CompositionPlan {
            composition_id: "test-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![
                managed_mod("entityculling", "entityculling"),
                managed_mod("c2me-fabric", "c2me-fabric"),
                managed_mod("moreculling", "moreculling"),
            ],
            jvm_preset: String::new(),
            fallback_chain: vec!["test-core".to_string()],
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install core fallback");

        assert_eq!(state.composition_id, "test-core");
        assert_eq!(state.tier, CompositionTier::Core);
        assert_eq!(state.failure_count, 0);
        assert_eq!(
            state
                .installed_mods
                .iter()
                .map(|installed| installed.project_id.as_str())
                .collect::<Vec<_>>(),
            vec!["lithium", "sodium"]
        );
        let loaded = load_state(&root)
            .expect("load state")
            .expect("fallback state should be saved");
        assert_eq!(loaded.composition_id, "test-core");
        assert!(root.join("sodium.jar").is_file());
        assert!(root.join("lithium.jar").is_file());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fallback_attempt_skips_emergency_disabled_artifact() {
        let root = test_root("install-fallback-skips-disabled-artifact");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_selective_modrinth_server(&["sodium", "lithium"]).await,
        )
        .expect("performance manager");
        set_test_compositions(
            &manager,
            vec![composition_def(
                "test-core",
                CompositionTier::Core,
                vec![
                    managed_mod("sodium", "sodium"),
                    managed_mod("lithium", "lithium"),
                ],
            )],
        );
        set_test_emergency_disables(
            &manager,
            vec![artifact_disable(
                "disable-lithium",
                "lithium",
                CompositionTier::Core,
            )],
        );
        let plan = CompositionPlan {
            composition_id: "test-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![
                managed_mod("entityculling", "entityculling"),
                managed_mod("c2me-fabric", "c2me-fabric"),
            ],
            jvm_preset: String::new(),
            fallback_chain: vec!["test-core".to_string()],
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install filtered fallback");

        assert_eq!(state.composition_id, "test-core");
        assert_eq!(
            state
                .installed_mods
                .iter()
                .map(|installed| installed.project_id.as_str())
                .collect::<Vec<_>>(),
            vec!["sodium"]
        );
        assert!(root.join("sodium.jar").is_file());
        assert!(!root.join("lithium.jar").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn partial_degradation_with_two_successes_does_not_fallback() {
        let root = test_root("install-partial-no-fallback");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_selective_modrinth_server(&["sodium", "lithium", "ferrite-core", "dynamic-fps"])
                .await,
        )
        .expect("performance manager");
        set_test_compositions(
            &manager,
            vec![composition_def(
                "test-core",
                CompositionTier::Core,
                vec![
                    managed_mod("ferrite-core", "ferrite-core"),
                    managed_mod("dynamic-fps", "dynamic-fps"),
                ],
            )],
        );
        let plan = CompositionPlan {
            composition_id: "test-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: vec![
                managed_mod("sodium", "sodium"),
                managed_mod("lithium", "lithium"),
                managed_mod("entityculling", "entityculling"),
            ],
            jvm_preset: String::new(),
            fallback_chain: vec!["test-core".to_string()],
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install degraded original");

        assert_eq!(state.composition_id, "test-extended");
        assert_eq!(state.tier, CompositionTier::Extended);
        assert_eq!(state.failure_count, 1);
        assert_eq!(state.installed_mods.len(), 2);
        assert!(root.join("sodium.jar").is_file());
        assert!(root.join("lithium.jar").is_file());
        assert!(!root.join("ferrite-core.jar").exists());
        assert!(!root.join("dynamic-fps.jar").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn vanilla_enhanced_fallback_saves_empty_state_and_removes_tracked_files() {
        let root = test_root("install-fallback-vanilla-empty");
        fs::write(root.join("managed.jar"), b"managed-v1").expect("write managed file");
        fs::write(root.join("user.jar"), b"user-v1").expect("write user file");
        save_state(
            &root,
            &test_state("old-core", vec![test_mod("sodium", "managed.jar")]),
        )
        .expect("save previous state");
        let manager = PerformanceManager::new_with_modrinth_base_url(
            spawn_selective_modrinth_server(&[]).await,
        )
        .expect("performance manager");
        set_test_compositions(
            &manager,
            vec![composition_def(
                "test-vanilla-enhanced",
                CompositionTier::VanillaEnhanced,
                Vec::new(),
            )],
        );
        let plan = CompositionPlan {
            composition_id: "test-core".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Core,
            mods: vec![managed_mod("entityculling", "entityculling")],
            jvm_preset: String::new(),
            fallback_chain: vec!["test-vanilla-enhanced".to_string()],
            warnings: Vec::new(),
            fallback_reason: String::new(),
        };

        let state = manager
            .ensure_installed(&plan, "1.20.4", &root)
            .await
            .expect("install vanilla fallback");

        assert_eq!(state.composition_id, "test-vanilla-enhanced");
        assert_eq!(state.tier, CompositionTier::VanillaEnhanced);
        assert!(state.installed_mods.is_empty());
        assert!(!root.join("managed.jar").exists());
        assert_eq!(
            fs::read(root.join("user.jar")).expect("read user"),
            b"user-v1"
        );
        let loaded = load_state(&root)
            .expect("load state")
            .expect("empty fallback state should be saved");
        assert!(loaded.installed_mods.is_empty());

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

    #[tokio::test]
    async fn remote_rules_refresh_request_failure_keeps_previous_rules_and_redacts_url() {
        let root = test_root("remote-refresh-request-redaction");
        let builtin = builtin_manifest().expect("builtin manifest");
        let (public_key, _) = signed_metadata(&builtin);
        let remote_base_url = spawn_closing_rules_server().await;
        let remote_url =
            format!("{remote_base_url}/private-feed/perf.json?api_token=secret-query-token");
        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some(remote_url.clone()),
            Some(public_key),
        )
        .expect("performance manager");
        let before = manager.rules_status();

        let after = manager
            .refresh_rules()
            .await
            .expect("refresh failure should expose status");
        let warning = after.warnings.join("\n");

        assert_eq!(after.rule_source, before.rule_source);
        assert_eq!(after.rule_channel, before.rule_channel);
        assert_eq!(after.generated_at, before.generated_at);
        assert_eq!(after.validation, RulesValidation::Valid);
        assert!(warning.contains("Remote rules refresh rejected: request failed"));
        assert!(!warning.contains(&remote_url));
        assert!(!warning.contains("127.0.0.1"));
        assert!(!warning.contains("private-feed"));
        assert!(!warning.contains("perf.json"));
        assert!(!warning.contains("api_token"));
        assert!(!warning.contains("secret-query-token"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remote_rules_refresh_cache_failure_keeps_previous_rules_and_redacts_path() {
        let root = test_root("remote-refresh-cache-path-secret");
        let mut remote = builtin_manifest().expect("builtin manifest");
        remote.generated_at = "2026-05-30T13:00:00Z".to_string();
        let (public_key, signature) = signed_metadata(&remote);
        let remote_url = spawn_remote_rules_server(remote, signature).await;
        let manager = PerformanceManager::new_with_config_dir_remote_url_and_public_key(
            &root,
            Some(remote_url),
            Some(public_key),
        )
        .expect("performance manager");
        let before = manager.rules_status();
        let cache_temp_path =
            crate::rules_cache::rules_cache_path(&root).with_extension("json.tmp");
        fs::create_dir_all(&cache_temp_path).expect("create blocking cache temp directory");

        let after = manager
            .refresh_rules()
            .await
            .expect("cache failure should expose status");
        let warning = after.warnings.join("\n");
        let synthetic = RulesRefreshError::Cache(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "failed to persist {}",
                root.join("local-path-secret/rules-cache.json").display()
            ),
        ));
        let synthetic_warning = remote_rules_refresh_warning("failed", &synthetic);

        assert_eq!(after.rule_source, before.rule_source);
        assert_eq!(after.rule_channel, before.rule_channel);
        assert_eq!(after.generated_at, before.generated_at);
        assert_eq!(after.validation, RulesValidation::Valid);
        assert!(
            warning
                .contains("Remote rules refresh failed: remote rules cache could not be persisted")
        );
        assert!(!warning.contains("remote-refresh-cache-path-secret"));
        assert!(!warning.contains("rules-cache.json.tmp"));
        assert!(!synthetic_warning.contains("local-path-secret"));
        assert!(!synthetic_warning.contains("rules-cache.json"));

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
        manager.record_refresh_warning(remote_rules_refresh_warning("rejected", &error));
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
            artifact_id: project_id.to_string(),
            project_id: project_id.to_string(),
            slug: slug.to_string(),
            name: "Declared Artifact".to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }

    fn composition_def(id: &str, tier: CompositionTier, mods: Vec<ManagedMod>) -> CompositionDef {
        CompositionDef {
            id: id.to_string(),
            display_name: id.to_string(),
            description: id.to_string(),
            families: vec![VersionFamily::F],
            loaders: vec!["fabric".to_string()],
            tier,
            mods,
            fallback_to: String::new(),
            jvm_preset: String::new(),
        }
    }

    fn set_test_compositions(manager: &PerformanceManager, compositions: Vec<CompositionDef>) {
        let mut active = manager
            .active
            .write()
            .expect("performance rules lock poisoned");
        active.manifest.compositions = compositions;
    }

    fn set_test_emergency_disables(
        manager: &PerformanceManager,
        emergency_disables: Vec<EmergencyDisable>,
    ) {
        let mut active = manager
            .active
            .write()
            .expect("performance rules lock poisoned");
        active.manifest.emergency_disables = emergency_disables;
    }

    fn artifact_disable(id: &str, target_id: &str, tier: CompositionTier) -> EmergencyDisable {
        EmergencyDisable {
            id: id.to_string(),
            target: EmergencyDisableTarget::Artifact,
            target_id: target_id.to_string(),
            reason: "test disable".to_string(),
            families: vec![VersionFamily::F],
            loaders: vec!["fabric".to_string()],
            tiers: vec![tier],
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

    async fn spawn_closing_rules_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind closing rules test server");
        let addr = listener.local_addr().expect("closing rules test addr");
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        format!("http://{addr}")
    }

    async fn spawn_remote_rules_server(
        manifest: crate::types::Manifest,
        signature: RulesSignatureMetadata,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind remote rules test server");
        let addr = listener.local_addr().expect("remote rules test addr");
        let body = serde_json::to_vec(&manifest).expect("serialize remote manifest");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
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

            let key_id = signature.key_id.unwrap_or_default();
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {}\r\n{}: {}\r\nconnection: close\r\n\r\n",
                body.len(),
                RULES_SIGNATURE_HEADER,
                signature.signature,
                RULES_KEY_ID_HEADER,
                key_id
            );
            if stream.write_all(headers.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.write_all(&body).await;
        });
        format!("http://{addr}/remote-rules/performance.json")
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

    fn sorted_projects(mut projects: Vec<String>) -> Vec<String> {
        projects.sort();
        projects
    }

    fn count_plan_mods_with_slug(mods: &[ManagedMod], slug: &str) -> usize {
        mods.iter()
            .filter(|managed_mod| managed_mod.slug == slug)
            .count()
    }

    async fn spawn_representative_modrinth_server(
        game_versions: &[&str],
        loaders: &[&str],
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind representative modrinth test server");
        let addr = listener.local_addr().expect("representative modrinth addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let game_versions = game_versions
            .iter()
            .map(|game_version| game_version.to_string())
            .collect::<Vec<_>>();
        let loaders = loaders
            .iter()
            .map(|loader| loader.to_string())
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let request_log = Arc::clone(&request_log);
                let game_versions = game_versions.clone();
                let loaders = loaders.clone();
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

                    let (status, content_type, body) = representative_modrinth_response(
                        &addr,
                        &first_line,
                        &game_versions,
                        &loaders,
                    );
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
        (format!("http://{addr}"), requests)
    }

    fn representative_modrinth_response(
        addr: &std::net::SocketAddr,
        first_line: &str,
        game_versions: &[String],
        loaders: &[String],
    ) -> (&'static str, &'static str, Vec<u8>) {
        if let Some(project) = representative_project_from_version_request(first_line) {
            return (
                "200 OK",
                "application/json",
                representative_version_response_body(addr, project, game_versions, loaders),
            );
        }
        if let Some(project) = representative_project_from_file_request(first_line) {
            return (
                "200 OK",
                "application/octet-stream",
                representative_file_body(project),
            );
        }
        ("404 Not Found", "text/plain", b"not found".to_vec())
    }

    fn representative_project_from_version_request(first_line: &str) -> Option<&str> {
        let start = first_line.find("/v2/project/")? + "/v2/project/".len();
        let rest = &first_line[start..];
        let end = rest.find("/version")?;
        Some(&rest[..end])
    }

    fn representative_project_from_file_request(first_line: &str) -> Option<&str> {
        let start = first_line.find("/files/")? + "/files/".len();
        let rest = &first_line[start..];
        let end = rest.find(".jar")?;
        Some(&rest[..end])
    }

    fn representative_version_response_body(
        addr: &std::net::SocketAddr,
        project_ref: &str,
        game_versions: &[String],
        loaders: &[String],
    ) -> Vec<u8> {
        let file_url = format!("http://{addr}/files/{project_ref}.jar");
        let sha512 = hex::encode(sha2::Sha512::digest(representative_file_body(project_ref)));
        let game_versions =
            serde_json::to_string(game_versions).expect("serialize representative game versions");
        let loaders = serde_json::to_string(loaders).expect("serialize representative loaders");
        format!(
            r#"[{{"id":"{project_ref}-representative-version","game_versions":{game_versions},"loaders":{loaders},"featured":true,"date_published":"2026-05-30T00:00:00Z","files":[{{"url":"{file_url}","filename":"{project_ref}.jar","primary":true,"hashes":{{"sha512":"{sha512}"}}}}]}}]"#
        )
        .into_bytes()
    }

    fn representative_file_body(project_ref: &str) -> Vec<u8> {
        format!("{project_ref}-representative-jar").into_bytes()
    }

    async fn spawn_selective_modrinth_server(success_projects: &[&str]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind selective modrinth test server");
        let addr = listener.local_addr().expect("selective modrinth test addr");
        let success_projects = success_projects
            .iter()
            .map(|project| project.to_string())
            .collect::<std::collections::HashSet<_>>();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let success_projects = success_projects.clone();
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
                    let (status, content_type, body) =
                        selective_modrinth_response(&addr, &success_projects, first_line);
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

    async fn spawn_leaky_download_failure_server() -> (String, Vec<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind leaky modrinth test server");
        let addr = listener.local_addr().expect("leaky modrinth test addr");
        let provider_url =
            format!("http://{addr}/private-provider/sodium-secret.jar?token=provider-secret");
        let leaked_details = vec![
            provider_url.clone(),
            "sodium-secret.jar".to_string(),
            "/home/zero/.minecraft/mods/private/sodium-secret.jar".to_string(),
            "C:\\Users\\Zero\\AppData\\Roaming\\.minecraft\\mods\\sodium-secret.jar".to_string(),
            "error decoding response body at line 1 column 2".to_string(),
            "No such file or directory (os error 2)".to_string(),
        ];
        let leaked_body = leaked_details.join("\n");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let provider_url = provider_url.clone();
                let leaked_body = leaked_body.clone();
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
                    let (status, content_type, body) = if first_line
                        .contains("/v2/project/sodium/version")
                    {
                        let body = format!(
                            r#"[{{"id":"version-a","game_versions":["1.20.4"],"loaders":["fabric"],"featured":true,"date_published":"2026-05-30T00:00:00Z","files":[{{"url":"{provider_url}","filename":"sodium-secret.jar","primary":true,"hashes":{{}}}}]}}]"#
                        );
                        ("200 OK", "application/json", body.into_bytes())
                    } else if first_line.contains("/private-provider/sodium-secret.jar") {
                        (
                            "500 Internal Server Error",
                            "text/plain",
                            leaked_body.into_bytes(),
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
        (format!("http://{addr}"), leaked_details)
    }

    fn selective_modrinth_response(
        addr: &std::net::SocketAddr,
        success_projects: &std::collections::HashSet<String>,
        first_line: &str,
    ) -> (&'static str, &'static str, Vec<u8>) {
        for project in success_projects {
            if first_line.contains(&format!("/v2/project/{project}/version")) {
                return (
                    "200 OK",
                    "application/json",
                    version_response_body(addr, project),
                );
            }
            if first_line.contains(&format!("/files/{project}.jar")) {
                return (
                    "200 OK",
                    "application/octet-stream",
                    format!("{project}-jar").into_bytes(),
                );
            }
        }

        ("404 Not Found", "text/plain", b"not found".to_vec())
    }

    async fn spawn_modrinth_server_with_sha512(sha512: Option<String>) -> String {
        spawn_modrinth_server_with_sha512_and_size(sha512, None).await
    }

    async fn spawn_modrinth_server_with_sha512_and_size(
        sha512: Option<String>,
        size: Option<u64>,
    ) -> String {
        spawn_modrinth_server_with_sha512_size_and_requests(sha512, size)
            .await
            .0
    }

    async fn spawn_modrinth_server_with_sha512_size_and_requests(
        sha512: Option<String>,
        size: Option<u64>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind modrinth test server");
        let addr = listener.local_addr().expect("modrinth test addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let sha512 = sha512.clone();
                let size = size;
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
                    let file_url = format!("http://{addr}/files/sodium.jar");
                    let hashes = if let Some(sha512) = sha512.as_ref() {
                        format!(r#""sha512":"{sha512}""#)
                    } else {
                        String::new()
                    };
                    let size = size
                        .map(|size| format!(r#","size":{size}"#))
                        .unwrap_or_default();
                    let (status, content_type, body) = if first_line
                        .contains("/v2/project/sodium/version")
                    {
                        let body = format!(
                            r#"[{{"id":"version-a","game_versions":["1.20.4"],"loaders":["fabric"],"featured":true,"date_published":"2026-05-30T00:00:00Z","files":[{{"url":"{file_url}","filename":"sodium.jar","primary":true,"hashes":{{{hashes}}}{size}}}]}}]"#
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
        (format!("http://{addr}"), requests)
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

    fn assert_no_replace_backups(root: &Path) {
        let entries = fs::read_dir(root).expect("read test root");
        for entry in entries {
            let entry = entry.expect("read test entry");
            assert!(
                !entry.file_name().to_string_lossy().contains(".replace-"),
                "replacement backup should be cleaned up: {:?}",
                entry.path()
            );
        }
    }
}
