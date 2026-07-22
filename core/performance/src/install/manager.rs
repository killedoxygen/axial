use super::model::{ActiveRules, InstallError};
use super::rules_refresh::{configured_remote_rules_url, normalize_remote_rules_url, rules_client};
use crate::resolve::{builtin_manifest, detect_hardware, resolve_plan};
use crate::rules_cache::{
    RulesCacheStartupSource, RulesCacheStatus, load_active_rules_cache,
};
use crate::signature::{RemoteRulesVerifier, configured_remote_rules_verifier};
use crate::storage::WeakManagedInstanceEffectAuthority;
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use crate::types::{CompositionPlan, ResolutionRequest};
use axial_fs::Directory;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub(super) const ACTIVE_RULES_LOCK_INVARIANT: &str = "active performance rules lock poisoned";

#[derive(Debug)]
pub struct PerformanceManager {
    pub(super) active: Arc<RwLock<ActiveRules>>,
    pub(super) rules_client: reqwest::Client,
    pub(super) remote_rules_url: Option<String>,
    pub(super) remote_rules_verifier: RemoteRulesVerifier,
    pub(super) rules_mutation_allowed: bool,
    pub(super) rules_cache_startup_source: RulesCacheStartupSource,
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
    instances_root_directory: Arc<Directory>,
    pub(super) instance_effect_authorities:
        Arc<Mutex<HashMap<String, WeakManagedInstanceEffectAuthority>>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ManagedInstanceIdentity {
    instance_id: Arc<str>,
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
        })
    }

    pub(super) fn instances_root_directory(&self) -> &Directory {
        &self.instances_root_directory
    }
}

impl ManagedInstanceIdentity {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
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
            rules_cache_startup_source: RulesCacheStartupSource::Synthetic,
            rules_authority_claimed: AtomicBool::new(false),
            managed_authority_claimed: AtomicBool::new(false),
        })
    }

    pub fn load_for_startup(performance_dir: &Path) -> Result<Self, InstallError> {
        Self::load_for_startup_with_remote_url(performance_dir, configured_remote_rules_url())
    }

    pub fn load_for_startup_with_remote_url(
        performance_dir: &Path,
        remote_rules_url: Option<String>,
    ) -> Result<Self, InstallError> {
        Self::load_for_startup_with_remote_url_and_public_key(
            performance_dir,
            remote_rules_url,
            std::env::var(crate::signature::PERFORMANCE_RULES_PUBLIC_KEY_ENV).ok(),
        )
    }

    pub fn load_for_startup_with_remote_url_and_public_key(
        performance_dir: &Path,
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
            performance_dir,
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
            rules_cache_startup_source: loaded.startup_source,
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
    ) -> Result<PerformanceRulesAuthority, std::io::Error> {
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
        instances_directory: Directory,
    ) -> Result<ManagedCompositionAuthority, std::io::Error> {
        instances_directory.identity()?;
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
            instances_root_directory: Arc::new(instances_directory),
            instance_effect_authorities: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}
