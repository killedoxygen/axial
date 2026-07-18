use super::model::{ActiveRules, InstallError};
use super::rules_refresh::{configured_remote_rules_url, normalize_remote_rules_url, rules_client};
use crate::resolve::{builtin_manifest, detect_hardware, resolve_plan};
use crate::rules_cache::{RulesCacheStatus, load_active_rules_cache};
use crate::signature::{RemoteRulesVerifier, configured_remote_rules_verifier};
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::{CompositionPlan, ResolutionRequest};
use axial_minecraft::managed_path::AnchoredDirectory;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

pub(super) const ACTIVE_RULES_LOCK_INVARIANT: &str = "active performance rules lock poisoned";

#[derive(Debug)]
pub struct PerformanceManager {
    pub(super) active: Arc<RwLock<ActiveRules>>,
    pub(super) rules_client: reqwest::Client,
    pub(super) remote_rules_url: Option<String>,
    pub(super) remote_rules_verifier: RemoteRulesVerifier,
    pub(super) rules_mutation_allowed: bool,
    rules_cache_path: Option<PathBuf>,
    rules_authority_claimed: AtomicBool,
    managed_authority_claimed: AtomicBool,
}

#[derive(Clone)]
pub struct PerformanceRulesAuthority {
    pub(super) manager: Arc<PerformanceManager>,
}

#[derive(Clone)]
pub struct ManagedCompositionAuthority {
    pub(super) manager: Arc<PerformanceManager>,
    instances_root: Arc<PathBuf>,
    instances_root_anchor: Arc<AnchoredDirectory>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ManagedInstanceIdentity {
    instance_id: Arc<str>,
    mods_dir: Arc<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedIdentityError {
    #[error("managed instance identity is not a canonical instance id")]
    InvalidInstanceId,
}

impl ManagedCompositionAuthority {
    pub fn identify(
        &self,
        instance_id: &str,
    ) -> Result<ManagedInstanceIdentity, ManagedIdentityError> {
        if !is_canonical_instance_id(instance_id) {
            return Err(ManagedIdentityError::InvalidInstanceId);
        }
        Ok(ManagedInstanceIdentity {
            instance_id: Arc::from(instance_id),
            mods_dir: Arc::new(self.instances_root.join(instance_id).join("mods")),
        })
    }

    pub(super) fn instances_root(&self) -> &Path {
        &self.instances_root
    }

    pub(super) fn instances_root_anchor(&self) -> &AnchoredDirectory {
        &self.instances_root_anchor
    }
}

impl ManagedInstanceIdentity {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub(super) fn mods_dir(&self) -> &Path {
        &self.mods_dir
    }
}

fn is_canonical_instance_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
            rules_client: rules_client(),
            remote_rules_url: None,
            remote_rules_verifier: RemoteRulesVerifier::disabled(),
            rules_mutation_allowed: true,
            rules_cache_path: None,
            rules_authority_claimed: AtomicBool::new(false),
            managed_authority_claimed: AtomicBool::new(false),
        })
    }

    pub fn load_for_startup(config_dir: &Path) -> Result<Self, InstallError> {
        Self::load_for_startup_with_remote_url(config_dir, configured_remote_rules_url())
    }

    pub fn load_for_startup_with_remote_url(
        config_dir: &Path,
        remote_rules_url: Option<String>,
    ) -> Result<Self, InstallError> {
        Self::load_for_startup_with_remote_url_and_public_key(
            config_dir,
            remote_rules_url,
            std::env::var(crate::signature::PERFORMANCE_RULES_PUBLIC_KEY_ENV).ok(),
        )
    }

    pub fn load_for_startup_with_remote_url_and_public_key(
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
            rules_client: rules_client(),
            remote_rules_url,
            remote_rules_verifier,
            rules_mutation_allowed: loaded.mutation_allowed,
            rules_cache_path: Some(crate::rules_cache::rules_cache_path(config_dir)),
            rules_authority_claimed: AtomicBool::new(false),
            managed_authority_claimed: AtomicBool::new(false),
        })
    }

    pub fn get_plan(&self, request: ResolutionRequest) -> CompositionPlan {
        let active = self.active.read().expect(ACTIVE_RULES_LOCK_INVARIANT);
        resolve_plan(Some(&active.manifest), request)
    }

    pub fn rules_status(&self) -> crate::status::PerformanceRulesStatus {
        let active = self.active.read().expect(ACTIVE_RULES_LOCK_INVARIANT);
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
    pub fn hardware(&self) -> crate::types::HardwareProfile {
        detect_hardware()
    }

    pub fn claim_rules_authority(
        self: &Arc<Self>,
        config_dir: &Path,
    ) -> Result<PerformanceRulesAuthority, std::io::Error> {
        let requested_path = crate::rules_cache::rules_cache_path(config_dir);
        if self.rules_cache_path.as_ref() != Some(&requested_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "performance rules authority path does not match startup admission",
            ));
        }
        self.rules_authority_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "performance rules authority is already claimed",
                )
            })?;
        Ok(PerformanceRulesAuthority {
            manager: self.clone(),
        })
    }

    pub fn claim_managed_authority(
        self: &Arc<Self>,
        instances_root: &Path,
    ) -> Result<ManagedCompositionAuthority, std::io::Error> {
        if !instances_root.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed composition instances root must be absolute",
            ));
        }
        match std::fs::symlink_metadata(instances_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "managed composition instances root must be a real directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(instances_root)?;
            }
            Err(error) => return Err(error),
        }
        let instances_root_anchor = AnchoredDirectory::open(instances_root)?;
        self.managed_authority_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "managed composition authority is already claimed",
                )
            })?;
        Ok(ManagedCompositionAuthority {
            manager: self.clone(),
            instances_root: Arc::new(instances_root.to_path_buf()),
            instances_root_anchor: Arc::new(instances_root_anchor),
        })
    }
}
