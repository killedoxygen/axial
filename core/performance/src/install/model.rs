use crate::rules_cache::RulesCacheStatus;
use crate::signature::{RulesSignatureError, RulesSignatureMetadata};
use crate::state::StateError;
use crate::status::{RuleChannel, RuleSource, RulesValidation};
use thiserror::Error;

pub const PERFORMANCE_RULES_URL_ENV: &str = "AXIAL_PERFORMANCE_RULES_URL";

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("failed to load performance manifest: {0}")]
    Manifest(#[from] crate::resolve::ResolveError),
    #[error("failed to access performance state: {0}")]
    State(#[from] StateError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("managed artifact staging failed")]
    Download(#[from] axial_minecraft::download::ExecutionDownloadError),
    #[error("composition plan is required")]
    MissingPlan,
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
pub(super) struct ActiveRules {
    pub(super) manifest: crate::types::Manifest,
    pub(super) rule_source: RuleSource,
    pub(super) rule_channel: RuleChannel,
    pub(super) rules_cache: RulesCacheStatus,
    pub(super) remote_refresh: bool,
    pub(super) last_refresh_at: Option<String>,
    pub(super) validation: RulesValidation,
}

#[derive(Debug, Clone)]
pub struct RemoteRulesCandidate {
    pub(super) manifest: crate::types::Manifest,
    pub(super) signature: RulesSignatureMetadata,
}

#[derive(Debug, Clone)]
pub struct VerifiedRemoteRules {
    pub(super) active: ActiveRules,
    pub(super) snapshot: crate::rules_cache::RulesCacheSnapshot,
}

impl VerifiedRemoteRules {
    pub fn snapshot(&self) -> &crate::rules_cache::RulesCacheSnapshot {
        &self.snapshot
    }
}
