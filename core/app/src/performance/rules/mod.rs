//! Active performance rules: the built-in manifest or a signed remote one.
//! A rule revision remains pinned throughout an accepted managed operation.

mod cache;
mod signature;
mod status;
#[cfg(test)]
mod tests;

pub use cache::RulesCacheSnapshot;
pub use signature::{RulesSignatureMetadata, canonical_manifest_payload};

use super::model::*;
use super::resolve::{builtin_manifest, detect_hardware, resolve_plan};
use crate::{
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{Connection, OptionalExtension},
    },
    tasks::{SpawnError, TaskOwner},
};
use chrono::Utc;
use signature::{KEY_ID_HEADER, RemoteRulesVerifier, RulesSignatureError, SIGNATURE_HEADER};
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

const ACTIVE_LOCK_INVARIANT: &str = "active performance rules lock poisoned";
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WARNING_CHARS: usize = 240;

pub const MIGRATION: Migration = Migration {
    id: "performance_rules.v1",
    sql: "CREATE TABLE performance_rules (singleton INTEGER PRIMARY KEY CHECK(singleton=1), snapshot BLOB NOT NULL CHECK(length(snapshot)<=1048576)) STRICT;",
};

/// Remote rules configuration, supplied once by the composition root.
#[derive(Clone, Debug)]
pub struct RulesSettings {
    remote: Option<RemoteSource>,
    refresh_interval: Duration,
}

#[derive(Clone, Debug)]
struct RemoteSource {
    url: String,
    verifier: RemoteRulesVerifier,
}

impl Default for RulesSettings {
    fn default() -> Self {
        Self {
            remote: None,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
        }
    }
}

impl RulesSettings {
    /// Blank values are unset. A configured source without a valid public key
    /// refuses every remote manifest. The background refresh interval defaults
    /// to six hours and is bounded to 15 minutes..24 hours.
    pub fn new(url: Option<&str>, public_key: Option<&str>, refresh_seconds: Option<&str>) -> Self {
        let remote = url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| RemoteSource {
                url: url.to_string(),
                verifier: RemoteRulesVerifier::from_public_key_hex(public_key),
            });
        let refresh_interval = refresh_seconds
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(DEFAULT_REFRESH_INTERVAL, |seconds| {
                Duration::from_secs(seconds).clamp(MIN_REFRESH_INTERVAL, MAX_REFRESH_INTERVAL)
            });
        Self {
            remote,
            refresh_interval,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RulesWorkflowError {
    #[error("performance rules storage is unavailable")]
    Storage(#[from] StorageError),
    #[error("performance rules could not be loaded")]
    Unavailable,
    #[error("performance rules changed or require settlement")]
    Changed,
    #[error("performance remote rules url is not configured")]
    Unconfigured,
    #[error("Performance rules provider response could not be verified. Try again later.")]
    ProviderFailed,
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("remote rules {0}")]
    Request(axial_minecraft::http::FetchError),
    #[error("remote rules returned HTTP {0}")]
    HttpStatus(u16),
    #[error("remote rules response is too large")]
    ResponseTooLarge,
    #[error("failed to parse remote performance manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("{0}")]
    Signature(#[from] RulesSignatureError),
}

impl From<axial_minecraft::http::FetchError> for FetchError {
    fn from(error: axial_minecraft::http::FetchError) -> Self {
        use axial_minecraft::http::FetchError;
        match error {
            FetchError::TooLarge { .. } | FetchError::Allocation => Self::ResponseTooLarge,
            error => match error.status() {
                Some(status) => Self::HttpStatus(status),
                None => Self::Request(error),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveRules {
    manifest: Manifest,
    source: RuleSource,
    cache: RulesCacheStatus,
}

impl ActiveRules {
    fn builtin(manifest: Manifest) -> Self {
        Self {
            manifest,
            source: RuleSource::BuiltIn,
            cache: RulesCacheStatus {
                recorded: false,
                state: RulesCacheState::Unavailable,
                updated_at: None,
                loaded_at: None,
                warning: None,
            },
        }
    }

    fn remote(snapshot: RulesCacheSnapshot) -> Self {
        Self {
            source: RuleSource::Remote,
            cache: RulesCacheStatus {
                recorded: true,
                state: RulesCacheState::Recorded,
                updated_at: Some(snapshot.updated_at),
                loaded_at: Some(Utc::now().to_rfc3339()),
                warning: None,
            },
            manifest: snapshot.manifest,
        }
    }
}

#[derive(Clone)]
pub struct PerformanceRules {
    active: Arc<RwLock<ActiveRules>>,
    settings: Arc<RulesSettings>,
    hardware: HardwareProfile,
    /// An unreadable cache is preserved, so managed work and refresh stay closed.
    mutation_allowed: bool,
    storage: Arc<MetadataStore>,
    gate: Arc<tokio::sync::RwLock<()>>,
    revision: Arc<AtomicU64>,
}

impl PerformanceRules {
    pub fn new(
        storage: Arc<MetadataStore>,
        settings: RulesSettings,
    ) -> Result<Self, RulesWorkflowError> {
        let manifest = builtin_manifest().map_err(|_| RulesWorkflowError::Unavailable)?;
        let mut active = ActiveRules::builtin(manifest);
        let mut mutation_allowed = true;
        if let Some(bytes) = storage.read(read_cache)? {
            let verified = settings
                .remote
                .as_ref()
                .and_then(|remote| RulesCacheSnapshot::decode_verified(&bytes, &remote.verifier));
            match verified {
                Some(snapshot) => active = ActiveRules::remote(snapshot),
                None => {
                    mutation_allowed = false;
                    active.cache.state = RulesCacheState::Invalid;
                    active.cache.warning = Some(
                        "Rules cache is invalid; previously stored bytes are preserved.".into(),
                    );
                }
            }
        }
        Ok(Self {
            active: Arc::new(RwLock::new(active)),
            settings: Arc::new(settings),
            hardware: detect_hardware(),
            mutation_allowed,
            storage,
            gate: Arc::new(tokio::sync::RwLock::new(())),
            revision: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn hardware(&self) -> HardwareProfile {
        self.hardware.clone()
    }

    pub fn uses_metadata(&self, metadata: &Arc<MetadataStore>) -> bool {
        Arc::ptr_eq(&self.storage, metadata)
    }

    /// Resolves against the active rules without pinning them.
    pub fn resolve(&self, request: ResolutionRequest) -> CompositionPlan {
        let active = self.active.read().expect(ACTIVE_LOCK_INVARIANT);
        resolve_plan(Some(&active.manifest), request)
    }

    pub async fn plan(
        &self,
        request: ResolutionRequest,
    ) -> Result<PlannedPerformance, RulesWorkflowError> {
        let lease = self.gate.clone().read_owned().await;
        if request.mode == PerformanceMode::Managed && !self.mutation_allowed {
            return Err(RulesWorkflowError::Changed);
        }
        let plan = self.resolve(request.clone());
        Ok(PlannedPerformance {
            plan,
            request,
            revision: self.revision.load(Ordering::Acquire),
            current: self.revision.clone(),
            _lease: lease,
        })
    }

    pub fn status(&self) -> PerformanceRulesStatusResponse {
        let active = self.active.read().expect(ACTIVE_LOCK_INVARIANT);
        status::response(&active, self.settings.remote.is_some())
    }

    /// The caller retains this idle worker; only accepted refreshes count as work.
    pub async fn run(&self, tasks: TaskOwner, mut shutdown: watch::Receiver<bool>) {
        if self.settings.remote.is_none() {
            return;
        }
        let interval = self.settings.refresh_interval;
        let mut changes = tasks.subscribe();
        loop {
            let stopping = *shutdown.borrow_and_update();
            if stopping || shutdown.has_changed().is_err() {
                return;
            }
            changes.borrow_and_update();
            let rules = self.clone();
            let mut attempt_shutdown = shutdown.clone();
            let accepted = tasks.try_spawn(self.clone(), move |cancel| async move {
                // Dropping a network/gate wait has no effects. Persistence and
                // active-rule publication in refresh contain no yielding gap.
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    _ = attempt_shutdown.wait_for(|stop| *stop) => None,
                    result = rules.refresh() => Some(result),
                }
            });
            let accepted = match accepted {
                Ok(accepted) => accepted,
                Err(SpawnError::Closed) => return,
                Err(SpawnError::AtCapacity) => {
                    tokio::select! {
                        _ = shutdown.wait_for(|stop| *stop) => return,
                        _ = changes.changed() => continue,
                    }
                }
                Err(error) => {
                    tracing::warn!(?error, "background rules refresh admission failed");
                    return;
                }
            };
            // TaskOwner retains the attempt even if this worker's waiter drops.
            match accepted.join().await {
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => {
                    // RulesWorkflowError's Display is fixed, safe public copy.
                    tracing::warn!(%error, "background rules refresh failed");
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(?error, "background rules refresh did not settle");
                    return;
                }
            }
            tokio::select! {
                _ = shutdown.wait_for(|stop| *stop) => return,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }

    pub async fn refresh(&self) -> Result<PerformanceRulesStatusResponse, RulesWorkflowError> {
        let _lease = self.gate.write().await;
        if !self.mutation_allowed {
            return Err(RulesWorkflowError::Changed);
        }
        let Some(remote) = self.settings.remote.as_ref() else {
            return Err(RulesWorkflowError::Unconfigured);
        };
        let snapshot = fetch(remote).await.map_err(|error| {
            self.active
                .write()
                .expect(ACTIVE_LOCK_INVARIANT)
                .cache
                .warning = Some(refresh_warning(&error));
            RulesWorkflowError::ProviderFailed
        })?;
        let bytes = snapshot
            .encode()
            .map_err(|_| RulesWorkflowError::Unavailable)?;
        self.storage.transaction(|tx| {
            if tx.execute("INSERT INTO performance_rules(singleton,snapshot) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET snapshot=excluded.snapshot", [&bytes])? != 1 {
                return Err(StorageError::Corrupt);
            }
            if read_cache(tx)?.as_deref() != Some(bytes.as_slice()) { return Err(StorageError::Corrupt); }
            Ok::<_, StorageError>(())
        })?;
        *self.active.write().expect(ACTIVE_LOCK_INVARIANT) = ActiveRules::remote(snapshot);
        self.revision.fetch_add(1, Ordering::AcqRel);
        Ok(self.status())
    }
}

fn read_cache(db: &Connection) -> Result<Option<Vec<u8>>, StorageError> {
    db.query_row(
        "SELECT snapshot FROM performance_rules WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(StorageError::from)
}

/// Rules are signature-verified, so an operator-configured plain-HTTP source remains admitted.
async fn fetch(remote: &RemoteSource) -> Result<RulesCacheSnapshot, FetchError> {
    remote.verifier.public_key()?;
    let body = axial_minecraft::http::send(
        axial_minecraft::http::artifact_client()
            .get(&remote.url)
            .timeout(FETCH_TIMEOUT),
        cache::MAX_BYTES as u64,
    )
    .await?;
    let header = |name| {
        body.headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let signature =
        signature::signature_metadata_from_header(header(SIGNATURE_HEADER), header(KEY_ID_HEADER))?;
    let manifest = serde_json::from_slice::<Manifest>(&body.bytes().await?)?;
    remote.verifier.verify_manifest(&manifest, &signature)?;
    Ok(RulesCacheSnapshot::remote(manifest, signature))
}

fn refresh_warning(error: &FetchError) -> String {
    let warning = match error {
        FetchError::Request(_) => {
            "Remote rules refresh failed: request failed; using previously active rules.".into()
        }
        error => format!("Remote rules refresh failed: {error}"),
    };
    let compact = warning.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_WARNING_CHARS {
        return compact;
    }
    let mut truncated = compact
        .chars()
        .take(MAX_WARNING_CHARS - 3)
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

pub struct PlannedPerformance {
    plan: CompositionPlan,
    request: ResolutionRequest,
    revision: u64,
    current: Arc<AtomicU64>,
    _lease: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl PlannedPerformance {
    pub fn plan(&self) -> &CompositionPlan {
        &self.plan
    }
    pub fn request(&self) -> &ResolutionRequest {
        &self.request
    }
    pub fn ensure_current(&self) -> Result<(), RulesWorkflowError> {
        if self.current.load(Ordering::Acquire) == self.revision {
            Ok(())
        } else {
            Err(RulesWorkflowError::Changed)
        }
    }
}
