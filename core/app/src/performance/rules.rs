//! A rule revision remains pinned throughout an accepted managed operation.

use super::model::*;
use crate::{
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{Connection, OptionalExtension, Transaction, params},
    },
    tasks::{CancellationToken, SpawnError, TaskOwner},
};
use axial_performance::{
    CompositionPlan, PerformanceManager, PerformanceRulesAuthority, ResolutionRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

const REFRESH_INTERVAL_ENV: &str = "AXIAL_PERFORMANCE_RULES_REFRESH_INTERVAL_SECONDS";

fn refresh_interval(value: Option<&str>) -> Duration {
    let seconds = value.and_then(|value| value.trim().parse::<u64>().ok());
    Duration::from_secs(seconds.unwrap_or(6 * 60 * 60).clamp(15 * 60, 24 * 60 * 60))
}

pub const MIGRATION: Migration = Migration {
    id: "performance_rules.v1",
    sql: "CREATE TABLE performance_rules (singleton INTEGER PRIMARY KEY CHECK(singleton=1), snapshot BLOB NOT NULL CHECK(length(snapshot)<=1048576)) STRICT;",
};

pub const IMPORT_MIGRATION: Migration = Migration {
    id: "performance_rules_imports.v1",
    sql: "CREATE TABLE performance_rules_imports (
        source_id TEXT PRIMARY KEY NOT NULL CHECK(length(source_id)=64),
        fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
        import_id TEXT UNIQUE NOT NULL CHECK(length(import_id)=64),
        receipt BLOB NOT NULL CHECK(length(receipt)<=65536)
    ) STRICT;",
};

/// The predecessor recorded sequence, not timestamps, for global refresh work.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct HistoricalRulesRefresh {
    pub operation_id: String,
    /// Decimal wire text preserves predecessor u64 sequences in JavaScript.
    #[serde(
        serialize_with = "serialize_refresh_sequence",
        deserialize_with = "deserialize_refresh_sequence"
    )]
    #[ts(type = "string")]
    pub sequence: u64,
    pub outcome: HistoricalRulesRefreshOutcome,
}

fn serialize_refresh_sequence<S: serde::Serializer>(
    sequence: &u64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&sequence.to_string())
}

fn deserialize_refresh_sequence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = String::deserialize(deserializer)?;
    let sequence = value
        .parse::<u64>()
        .map_err(|_| serde::de::Error::custom("invalid refresh sequence"))?;
    if sequence == 0 || sequence.to_string() != value {
        return Err(serde::de::Error::custom("invalid refresh sequence"));
    }
    Ok(sequence)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalRulesRefreshOutcome {
    Succeeded {
        cache_changed: bool,
    },
    Failed {
        failure_point: HistoricalRulesRefreshFailure,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub enum HistoricalRulesRefreshFailure {
    #[serde(rename = "refresh_remote_rules")]
    RemoteRules,
    #[serde(rename = "refresh_rules_journal_reconciliation")]
    JournalReconciliation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct RulesImportReceipt {
    pub rules_import_id: String,
    pub fingerprint: String,
    pub cache_sha256: Option<String>,
    pub refresh_history: Vec<HistoricalRulesRefresh>,
}

pub struct RulesImportCommit {
    pub receipt: RulesImportReceipt,
    pub already_imported: bool,
    pub stored_cache_matches_import: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RulesImportError {
    #[error("The predecessor rules data is malformed or unsupported.")]
    Invalid,
    #[error("The destination rules signing key and remote policy must be configured.")]
    Unconfigured,
    #[error("The predecessor rules are not trusted by this destination.")]
    Untrusted,
    #[error("The rules import conflicts with existing destination data.")]
    Conflict,
    #[error("Rules changes are unavailable until existing rejected data is resolved.")]
    Unavailable,
    #[error("The rules import was cancelled before publication.")]
    Cancelled,
    #[error("Rules import storage is unavailable.")]
    Storage(#[from] StorageError),
}
impl From<crate::storage::rusqlite::Error> for RulesImportError {
    fn from(error: crate::storage::rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone)]
pub(crate) struct PreparedRules {
    source_id: String,
    cache: Option<Arc<[u8]>>,
    receipt: RulesImportReceipt,
    encoded: Arc<[u8]>,
}
impl PreparedRules {
    pub(crate) fn receipt(&self) -> &RulesImportReceipt {
        &self.receipt
    }
}

/// A completed fact is immutable even when active rules later refresh. This
/// read-only proof can be rechecked in an instance's publication transaction.
#[derive(Clone)]
pub(crate) struct CompletedRulesImport {
    source_id: String,
    receipt: Arc<RulesImportReceipt>,
}
impl CompletedRulesImport {
    pub(crate) fn receipt(&self) -> &RulesImportReceipt {
        &self.receipt
    }
    pub(crate) fn matches(&self, source_id: &str, fingerprint: &str) -> bool {
        self.source_id == source_id && self.receipt.fingerprint == fingerprint
    }
    pub(crate) fn verify_in(&self, tx: &Transaction<'_>) -> Result<(), RulesImportError> {
        if read_import(tx, &self.receipt.rules_import_id)?.as_ref()
            != Some(&(self.source_id.clone(), (*self.receipt).clone()))
        {
            return Err(RulesImportError::Conflict);
        }
        Ok(())
    }
}

pub(crate) fn import_id(source: &str, fingerprint: &str) -> Result<String, RulesImportError> {
    if !digest(source) || !digest(fingerprint) {
        return Err(RulesImportError::Invalid);
    }
    Ok(hex::encode(Sha256::digest(
        format!("axial.rules-import.v1\0{source}\0{fingerprint}").as_bytes(),
    )))
}

pub(crate) fn prepare_import(
    source: &str,
    fingerprint: &str,
    cache: Option<Vec<u8>>,
    mut history: Vec<HistoricalRulesRefresh>,
) -> Result<PreparedRules, RulesImportError> {
    if cache.is_none() && history.is_empty() {
        return Err(RulesImportError::Invalid);
    }
    if let Some(bytes) = &cache {
        PerformanceRulesAuthority::decode_cached_rules(bytes)
            .map_err(|_| RulesImportError::Invalid)?;
    }
    history.sort_by_key(|record| record.sequence);
    validate_history(&history)?;
    let receipt = RulesImportReceipt {
        rules_import_id: import_id(source, fingerprint)?,
        fingerprint: fingerprint.into(),
        cache_sha256: cache
            .as_ref()
            .map(|bytes| hex::encode(Sha256::digest(bytes))),
        refresh_history: history,
    };
    let encoded = serde_json::to_vec(&receipt).map_err(|_| RulesImportError::Invalid)?;
    if encoded.len() > 65536 {
        return Err(RulesImportError::Invalid);
    }
    Ok(PreparedRules {
        source_id: source.into(),
        cache: cache.map(Into::into),
        receipt,
        encoded: encoded.into(),
    })
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_history(history: &[HistoricalRulesRefresh]) -> Result<(), RulesImportError> {
    let mut ids = BTreeSet::new();
    let mut previous = 0;
    if history.len() > 128 {
        return Err(RulesImportError::Invalid);
    }
    for record in history {
        let id = record
            .operation_id
            .strip_prefix("op-")
            .and_then(|id| uuid::Uuid::parse_str(id).ok());
        if id.is_none_or(|id| {
            id.get_version() != Some(uuid::Version::Random)
                || id.get_variant() != uuid::Variant::RFC4122
                || format!("op-{id}") != record.operation_id
        }) || !ids.insert(&record.operation_id)
            || record.sequence <= previous
        {
            return Err(RulesImportError::Invalid);
        }
        previous = record.sequence;
    }
    Ok(())
}

fn read_import(
    db: &Connection,
    id: &str,
) -> Result<Option<(String, RulesImportReceipt)>, RulesImportError> {
    let row: Option<(String, String, Vec<u8>)> = db.query_row(
        "SELECT source_id,fingerprint,receipt FROM performance_rules_imports WHERE import_id=?1", [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    row.map(|(source, fingerprint, bytes)| {
        if bytes.len() > 65536 {
            return Err(RulesImportError::Conflict);
        }
        let receipt: RulesImportReceipt =
            serde_json::from_slice(&bytes).map_err(|_| RulesImportError::Conflict)?;
        if import_id(&source, &fingerprint).map_err(|_| RulesImportError::Conflict)? != id
            || receipt.rules_import_id != id
            || receipt.fingerprint != fingerprint
            || receipt
                .cache_sha256
                .as_deref()
                .is_some_and(|hash| !digest(hash))
            || (receipt.cache_sha256.is_none() && receipt.refresh_history.is_empty())
        {
            return Err(RulesImportError::Conflict);
        }
        validate_history(&receipt.refresh_history).map_err(|_| RulesImportError::Conflict)?;
        Ok((source, receipt))
    })
    .transpose()
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

fn cache_matches(receipt: &RulesImportReceipt, bytes: Option<&[u8]>) -> bool {
    receipt
        .cache_sha256
        .as_ref()
        .zip(bytes)
        .is_some_and(|(expected, bytes)| *expected == hex::encode(Sha256::digest(bytes)))
}

#[derive(Debug, thiserror::Error)]
pub enum RulesWorkflowError {
    #[error("performance rules storage is unavailable")]
    Storage(#[from] StorageError),
    #[error("performance rules could not be loaded")]
    Unavailable,
    #[error("performance rules changed or require settlement")]
    Changed,
    #[error("performance rules refresh failed; previously active rules remain selected")]
    RefreshFailed,
}

#[derive(Clone)]
pub struct PerformanceRules {
    manager: Arc<PerformanceManager>,
    authority: PerformanceRulesAuthority,
    storage: Arc<MetadataStore>,
    gate: Arc<tokio::sync::RwLock<()>>,
    revision: Arc<AtomicU64>,
}

impl PerformanceRules {
    pub fn new(storage: Arc<MetadataStore>) -> Result<Self, RulesWorkflowError> {
        Self::with_remote(
            storage,
            std::env::var(axial_performance::PERFORMANCE_RULES_URL_ENV).ok(),
            std::env::var(axial_performance::PERFORMANCE_RULES_PUBLIC_KEY_ENV).ok(),
        )
    }

    pub fn with_remote(
        storage: Arc<MetadataStore>,
        remote_url: Option<String>,
        public_key: Option<String>,
    ) -> Result<Self, RulesWorkflowError> {
        let bytes: Option<Vec<u8>> = storage.read(|connection| {
            connection
                .query_row(
                    "SELECT snapshot FROM performance_rules WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(StorageError::from)
        })?;
        let manager = Arc::new(
            PerformanceManager::from_cached_rules(bytes.as_deref(), remote_url, public_key)
                .map_err(|_| RulesWorkflowError::Unavailable)?,
        );
        let authority = manager
            .claim_rules_authority()
            .map_err(|_| RulesWorkflowError::Unavailable)?;
        Ok(Self {
            manager,
            authority,
            storage,
            gate: Arc::new(tokio::sync::RwLock::new(())),
            revision: Arc::new(AtomicU64::new(1)),
        })
    }

    pub(crate) fn manager(&self) -> &Arc<PerformanceManager> {
        &self.manager
    }

    pub fn uses_metadata(&self, metadata: &Arc<MetadataStore>) -> bool {
        Arc::ptr_eq(&self.storage, metadata)
    }

    pub async fn plan(
        &self,
        request: ResolutionRequest,
    ) -> Result<PlannedPerformance, RulesWorkflowError> {
        let lease = self.gate.clone().read_owned().await;
        if request.mode == PerformanceMode::Managed && !self.authority.mutation_allowed() {
            return Err(RulesWorkflowError::Changed);
        }
        let plan = self.manager.get_plan(request.clone());
        Ok(PlannedPerformance {
            plan,
            request,
            revision: self.revision.load(Ordering::Acquire),
            current: self.revision.clone(),
            _lease: lease,
        })
    }

    pub fn status(&self) -> PerformanceRulesStatusResponse {
        rules_response(self.manager.rules_status())
    }

    /// The caller retains this idle worker; only accepted refreshes count as work.
    pub async fn run(&self, tasks: TaskOwner, mut shutdown: watch::Receiver<bool>) {
        if !self.manager.remote_refresh_enabled() {
            return;
        }
        let interval = refresh_interval(std::env::var(REFRESH_INTERVAL_ENV).ok().as_deref());
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

    pub fn import_status(
        &self,
        id: &str,
    ) -> Result<(Option<RulesImportReceipt>, bool), RulesImportError> {
        if !digest(id) {
            return Err(RulesImportError::Invalid);
        }
        self.storage.read(|db| {
            let receipt = read_import(db, id)?.map(|(_, receipt)| receipt);
            let cache = read_cache(db)?;
            let matches = receipt
                .as_ref()
                .is_some_and(|receipt| cache_matches(receipt, cache.as_deref()));
            Ok((receipt, matches))
        })
    }

    pub(crate) fn completed_import(
        &self,
        source: &str,
        fingerprint: &str,
    ) -> Result<Option<CompletedRulesImport>, RulesImportError> {
        let id = import_id(source, fingerprint)?;
        self.storage.read(|db| {
            Ok(
                read_import(db, &id)?.map(|(source_id, receipt)| CompletedRulesImport {
                    source_id,
                    receipt: Arc::new(receipt),
                }),
            )
        })
    }

    pub(crate) async fn commit_import<E: From<RulesImportError>>(
        &self,
        prepared: &PreparedRules,
        cancel: &CancellationToken,
        check_source: impl Future<Output = Result<(), E>>,
    ) -> Result<RulesImportCommit, E> {
        let _lease = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(RulesImportError::Cancelled.into()),
            lease = self.gate.write() => lease,
        };
        check_source.await?;
        if cancel.is_cancelled() {
            return Err(RulesImportError::Cancelled.into());
        }
        let (existing, cache) = self
            .storage
            .read(|db| {
                Ok::<_, RulesImportError>((
                    read_import(db, &prepared.receipt.rules_import_id)?,
                    read_cache(db)?,
                ))
            })
            .map_err(E::from)?;
        if let Some((source, receipt)) = existing {
            if source != prepared.source_id || receipt != prepared.receipt {
                return Err(RulesImportError::Conflict.into());
            }
            return Ok(RulesImportCommit {
                stored_cache_matches_import: cache_matches(&receipt, cache.as_deref()),
                receipt,
                already_imported: true,
            });
        }
        let candidate = if let Some(bytes) = &prepared.cache {
            if !self.authority.mutation_allowed() {
                return Err(RulesImportError::Unavailable.into());
            }
            let candidate = self.authority.verify_cached_rules(bytes).map_err(|error| {
                E::from(match error {
                    axial_performance::RulesRefreshError::Unconfigured => {
                        RulesImportError::Unconfigured
                    }
                    axial_performance::RulesRefreshError::Signature(
                        axial_performance::RulesSignatureError::MissingPublicKey
                        | axial_performance::RulesSignatureError::InvalidPublicKey,
                    ) => RulesImportError::Unconfigured,
                    _ => RulesImportError::Untrusted,
                })
            })?;
            if cache
                .as_deref()
                .is_some_and(|existing| existing != bytes.as_ref())
            {
                return Err(RulesImportError::Conflict.into());
            }
            // Exact bytes are a no-op only if the existing owner really loaded
            // them; an out-of-band singleton edit cannot create live authority.
            if cache.is_some() && !self.authority.matches_loaded_cache(bytes) {
                return Err(RulesImportError::Unavailable.into());
            }
            Some(candidate)
        } else {
            None
        };
        let adopt = candidate.is_some() && cache.is_none();
        let persist = async {
            self.storage.transaction(|tx| {
                if cancel.is_cancelled() { return Err(RulesImportError::Cancelled); }
                if read_cache(tx)? != cache || read_import(tx, &prepared.receipt.rules_import_id)?.is_some() {
                    return Err(RulesImportError::Conflict);
                }
                let same_source: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM performance_rules_imports WHERE source_id=?1)", [&prepared.source_id], |row| row.get(0))?;
                if same_source { return Err(RulesImportError::Conflict); }
                if adopt && tx.execute("INSERT INTO performance_rules(singleton,snapshot) VALUES(1,?1)", [prepared.cache.as_deref().ok_or(RulesImportError::Invalid)?])? != 1 {
                    return Err(RulesImportError::Conflict);
                }
                if tx.execute("INSERT INTO performance_rules_imports(source_id,fingerprint,import_id,receipt) VALUES(?1,?2,?3,?4)", params![prepared.source_id, prepared.receipt.fingerprint, prepared.receipt.rules_import_id, prepared.encoded.as_ref()])? != 1 {
                    return Err(RulesImportError::Conflict);
                }
                let expected_cache = if adopt { prepared.cache.as_deref() } else { cache.as_deref() };
                if read_cache(tx)?.as_deref() != expected_cache
                    || read_import(tx, &prepared.receipt.rules_import_id)? != Some((prepared.source_id.clone(), prepared.receipt.clone())) {
                    return Err(RulesImportError::Conflict);
                }
                if cancel.is_cancelled() { return Err(RulesImportError::Cancelled); }
                Ok(())
            })
        };
        if adopt {
            self.authority
                .settle_remote_rules(
                    candidate.ok_or_else(|| E::from(RulesImportError::Invalid))?,
                    persist,
                )
                .await
                .map_err(E::from)?;
            self.revision.fetch_add(1, Ordering::AcqRel);
        } else {
            persist.await.map_err(E::from)?;
        }
        // Persistence and in-memory publication above contain no yielding step.
        // Cancellation after the commit must not disguise a successful import.
        Ok(RulesImportCommit {
            receipt: prepared.receipt.clone(),
            already_imported: false,
            stored_cache_matches_import: prepared.cache.is_some(),
        })
    }

    pub async fn refresh(&self) -> Result<PerformanceRulesStatusResponse, RulesWorkflowError> {
        let _lease = self.gate.write().await;
        if !self.authority.mutation_allowed() {
            return Err(RulesWorkflowError::Changed);
        }
        let candidate = self.authority.fetch_remote_rules().await.map_err(|error| {
            self.authority
                .record_refresh_warning(axial_performance::remote_rules_refresh_warning(
                    "failed", &error,
                ));
            RulesWorkflowError::RefreshFailed
        })?;
        let bytes = candidate
            .snapshot()
            .encode()
            .map_err(|_| RulesWorkflowError::Unavailable)?;
        let status = self.authority.settle_remote_rules(candidate, async {
            self.storage.transaction(|tx| {
                if tx.execute("INSERT INTO performance_rules(singleton,snapshot) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET snapshot=excluded.snapshot", [&bytes])? != 1 {
                    return Err(StorageError::Corrupt);
                }
                if read_cache(tx)?.as_deref() != Some(bytes.as_slice()) { return Err(StorageError::Corrupt); }
                Ok::<_, StorageError>(())
            })
        }).await?;
        self.revision.fetch_add(1, Ordering::AcqRel);
        Ok(rules_response(status))
    }
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

fn rules_response(status: PerformanceRulesStatus) -> PerformanceRulesStatusResponse {
    let valid = status.validation == RulesValidation::Valid
        && status.rules_cache.state != RulesCacheState::Invalid;
    let view_model = PerformanceRulesStatusViewModel {
        source_label: match status.rule_source {
            RuleSource::BuiltIn => "Built-in rules",
            RuleSource::Remote => "Remote rules",
        }
        .into(),
        channel_label: match status.rule_channel {
            RuleChannel::Bundled => "Bundled",
            RuleChannel::Local => "Local",
            RuleChannel::Remote => "Remote",
        }
        .into(),
        validation_label: if valid { "Valid" } else { "Needs attention" }.into(),
        validation_tone: if valid {
            ViewModelTone::Ok
        } else {
            ViewModelTone::Warn
        },
        validation_icon: if valid { "check" } else { "alert-triangle" }.into(),
        summary: format!("{} performance compositions", status.composition_count),
        refresh_label: if status.remote_refresh {
            "Refresh rules"
        } else {
            "Remote refresh is not configured"
        }
        .into(),
        generated_label: status.generated_at.clone(),
        cache_label: if status.rules_cache.recorded {
            "Cached"
        } else {
            "No verified remote cache"
        }
        .into(),
        emergency_disable_label: format!("{} emergency disables", status.emergency_disable_count),
        details_label: "Performance rules".into(),
        health_states_label: "Healthy, disabled, invalid".into(),
        ownership_label: "Composition-managed files and user-managed files".into(),
        warnings: status.warnings.clone(),
    };
    PerformanceRulesStatusResponse { status, view_model }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn signed_cache(time: &str) -> (Vec<u8>, String) {
        let key = SigningKey::from_bytes(&[23; 32]);
        let mut manifest = axial_performance::builtin_manifest().unwrap();
        manifest.generated_at = time.into();
        let signature =
            key.sign(&axial_performance::canonical_manifest_payload(&manifest).unwrap());
        let snapshot = axial_performance::RulesCacheSnapshot {
            rule_source: RuleSource::Remote,
            rule_channel: RuleChannel::Remote,
            schema_version: manifest.schema_version,
            generated_at: time.into(),
            validation: RulesValidation::Valid,
            updated_at: time.into(),
            manifest,
            signature: axial_performance::RulesSignatureMetadata {
                signature: hex::encode(signature.to_bytes()),
                key_id: Some("retained-fixture".into()),
            },
        };
        (
            snapshot.encode().unwrap(),
            hex::encode(key.verifying_key().to_bytes()),
        )
    }

    fn fixture() -> (Arc<MetadataStore>, PerformanceRules, PreparedRules) {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage.migrate(&[MIGRATION, IMPORT_MIGRATION]).unwrap();
        let (bytes, key) = signed_cache("2001-01-01T00:00:00Z");
        let rules = PerformanceRules::with_remote(
            storage.clone(),
            Some("https://example.invalid/rules".into()),
            Some(key),
        )
        .unwrap();
        let prepared =
            prepare_import(&"1".repeat(64), &"2".repeat(64), Some(bytes), Vec::new()).unwrap();
        (storage, rules, prepared)
    }

    async fn commit(
        rules: &PerformanceRules,
        prepared: &PreparedRules,
    ) -> Result<RulesImportCommit, RulesImportError> {
        rules
            .commit_import(prepared, &CancellationToken::new(), async { Ok(()) })
            .await
    }

    async fn refresh_fixture() -> (
        Arc<MetadataStore>,
        PerformanceRules,
        tokio::net::TcpListener,
        axial_performance::RulesCacheSnapshot,
    ) {
        let (storage, _, prepared) = fixture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (next, key) = signed_cache("2002-01-01T00:00:00Z");
        let rules = PerformanceRules::with_remote(
            storage.clone(),
            Some(format!("http://{}/rules", listener.local_addr().unwrap())),
            Some(key),
        )
        .unwrap();
        commit(&rules, &prepared).await.unwrap();
        (
            storage,
            rules,
            listener,
            serde_json::from_slice(&next).unwrap(),
        )
    }

    async fn respond_rules(
        mut stream: tokio::net::TcpStream,
        snapshot: &axial_performance::RulesCacheSnapshot,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let body = serde_json::to_vec(&snapshot.manifest).unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {}\r\nConnection: close\r\n\r\n",
            body.len(),
            snapshot.signature.signature,
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
    }

    async fn refresh_idle(tasks: &crate::tasks::TaskOwner) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn start_refresh(
        rules: &PerformanceRules,
        tasks: &TaskOwner,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (shutdown, receiver) = watch::channel(false);
        let rules = rules.clone();
        let tasks = tasks.clone();
        let worker = tokio::spawn(async move { rules.run(tasks, receiver).await });
        (shutdown, worker)
    }

    async fn accept_refresh(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
        tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap()
            .0
    }

    #[test]
    fn periodic_rules_interval_preserves_legacy_default_and_bounds() {
        for (value, seconds) in [
            (None, 21_600),
            (Some(""), 21_600),
            (Some(" \t\n"), 21_600),
            (Some("invalid"), 21_600),
            (Some("-1"), 21_600),
            (Some("18446744073709551616"), 21_600),
            (Some("0"), 900),
            (Some("1"), 900),
            (Some("900"), 900),
            (Some(" 1800 "), 1_800),
            (Some("21600"), 21_600),
            (Some("86400"), 86_400),
            (Some("86401"), 86_400),
            (Some("18446744073709551615"), 86_400),
        ] {
            assert_eq!(refresh_interval(value), Duration::from_secs(seconds));
        }
    }

    #[tokio::test]
    async fn periodic_rules_refreshes_configured_signed_provider_immediately() {
        let (storage, rules, listener, next) = refresh_fixture().await;
        let tasks = crate::tasks::TaskOwner::new(1).unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let worker = {
            let rules = rules.clone();
            let tasks = tasks.clone();
            tokio::spawn(async move { rules.run(tasks, receiver).await })
        };
        let accepted =
            tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await;
        if accepted.is_err() {
            shutdown.send_replace(true);
            worker.await.unwrap();
            panic!("configured background rules refresh did not contact its provider");
        }
        let (stream, _) = accepted.unwrap().unwrap();
        assert!(!tasks.status().is_idle());
        respond_rules(stream, &next).await;
        refresh_idle(&tasks).await;
        tokio::task::yield_now().await;
        assert!(!worker.is_finished(), "the periodic worker remains asleep");
        tasks.try_close_idle().unwrap();
        shutdown.send_replace(true);
        worker.await.unwrap();
        assert_eq!(rules.status().status.generated_at, next.generated_at);
        let saved = storage.read(read_cache).unwrap().unwrap();
        let saved: axial_performance::RulesCacheSnapshot = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved.manifest, next.manifest);
        assert_eq!(rules.revision.load(Ordering::Acquire), 3);
        tasks.try_close_idle().unwrap();
    }

    #[tokio::test]
    async fn periodic_rules_delay_starts_after_completion_and_failed_refresh_preserves_cache() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (storage, rules, listener, next) = refresh_fixture().await;
        let tasks = TaskOwner::new(1).unwrap();
        let gate = rules.gate.write().await;
        let (shutdown, worker) = start_refresh(&rules, &tasks);
        tokio::time::timeout(Duration::from_secs(1), async {
            while tasks.status().is_idle() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let interval = refresh_interval(std::env::var(REFRESH_INTERVAL_ENV).ok().as_deref());
        tokio::time::pause();
        tokio::time::advance(interval + Duration::from_secs(10)).await;
        tokio::time::resume();
        drop(gate);
        respond_rules(accept_refresh(&listener).await, &next).await;
        refresh_idle(&tasks).await;
        tokio::task::yield_now().await;
        let saved = storage.read(read_cache).unwrap();
        let revision = rules.revision.load(Ordering::Acquire);

        tokio::time::pause();
        tokio::time::advance(interval - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(tasks.status().is_idle(), "no fixed-rate catch-up attempt");
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::time::resume();
        let mut stream = accept_refresh(&listener).await;
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        refresh_idle(&tasks).await;
        assert_eq!(storage.read(read_cache).unwrap(), saved);
        assert_eq!(rules.revision.load(Ordering::Acquire), revision);
        assert_eq!(rules.status().status.generated_at, next.generated_at);
        assert!(!rules.status().status.warnings.is_empty());
        shutdown.send_replace(true);
        worker.await.unwrap();
        tasks.try_close_idle().unwrap();
    }

    #[tokio::test]
    async fn periodic_rules_skips_unconfigured_closed_and_stopped_admission() {
        use futures_util::FutureExt;
        for case in ["unconfigured", "stopped", "watch_closed", "owner_closed"] {
            let (storage, mut rules, listener, _) = refresh_fixture().await;
            let saved = storage.read(read_cache).unwrap();
            let tasks = TaskOwner::new(1).unwrap();
            let (shutdown, receiver) = watch::channel(case == "stopped");
            if case == "unconfigured" {
                rules = PerformanceRules::with_remote(storage.clone(), None, None).unwrap();
            }
            if case == "watch_closed" {
                drop(shutdown);
            }
            if case == "owner_closed" {
                tasks.try_close_idle().unwrap();
            }
            tokio::time::timeout(Duration::from_secs(1), rules.run(tasks.clone(), receiver))
                .await
                .unwrap();
            assert!(listener.accept().now_or_never().is_none(), "{case}");
            assert!(tasks.status().is_idle(), "{case}");
            assert_eq!(storage.read(read_cache).unwrap(), saved, "{case}");
        }
    }

    #[tokio::test]
    async fn periodic_rules_waits_for_capacity_then_refreshes() {
        use futures_util::FutureExt;
        let (storage, rules, listener, next) = refresh_fixture().await;
        let tasks = TaskOwner::new(1).unwrap();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let occupied = tasks
            .try_spawn((), |_| async move { held.await.unwrap() })
            .unwrap();
        let (shutdown, worker) = start_refresh(&rules, &tasks);
        tokio::task::yield_now().await;
        assert!(!worker.is_finished());
        assert!(listener.accept().now_or_never().is_none());
        release.send(()).unwrap();
        occupied.join().await.unwrap();
        respond_rules(accept_refresh(&listener).await, &next).await;
        refresh_idle(&tasks).await;
        shutdown.send_replace(true);
        worker.await.unwrap();
        assert_eq!(rules.status().status.generated_at, next.generated_at);
        let saved: axial_performance::RulesCacheSnapshot =
            serde_json::from_slice(&storage.read(read_cache).unwrap().unwrap()).unwrap();
        assert_eq!(saved.manifest, next.manifest);
        tasks.try_close_idle().unwrap();
    }

    #[tokio::test]
    async fn periodic_rules_dropped_waiter_keeps_accepted_refresh_owned_through_publication() {
        let (storage, rules, listener, next) = refresh_fixture().await;
        let tasks = TaskOwner::new(1).unwrap();
        let (_shutdown, worker) = start_refresh(&rules, &tasks);
        let stream = accept_refresh(&listener).await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(!tasks.status().is_idle());
        assert!(tasks.try_close_idle().is_err());
        assert!(rules.gate.try_write().is_err());
        respond_rules(stream, &next).await;
        refresh_idle(&tasks).await;
        assert!(rules.gate.try_write().is_ok());
        assert_eq!(rules.status().status.generated_at, next.generated_at);
        let saved: axial_performance::RulesCacheSnapshot =
            serde_json::from_slice(&storage.read(read_cache).unwrap().unwrap()).unwrap();
        assert_eq!(saved.manifest, next.manifest);
        assert_eq!(rules.revision.load(Ordering::Acquire), 3);
        tasks.try_close_idle().unwrap();
    }

    #[tokio::test]
    async fn periodic_rules_pending_refresh_cancels_without_partial_publication() {
        use futures_util::FutureExt;
        for waiting_on_gate in [true, false] {
            for owner_shutdown in [true, false] {
                let (storage, rules, listener, _) = refresh_fixture().await;
                let saved = storage.read(read_cache).unwrap();
                let revision = rules.revision.load(Ordering::Acquire);
                let generated_at = rules.status().status.generated_at;
                let tasks = TaskOwner::new(1).unwrap();
                let gate = if waiting_on_gate {
                    Some(rules.gate.write().await)
                } else {
                    None
                };
                let (shutdown, worker) = start_refresh(&rules, &tasks);
                let stream = if waiting_on_gate {
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while tasks.status().is_idle() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    None
                } else {
                    Some(accept_refresh(&listener).await)
                };
                assert!(tasks.try_close_idle().is_err());
                if owner_shutdown {
                    tasks.shutdown(Duration::from_secs(1)).await.unwrap();
                } else {
                    shutdown.send_replace(true);
                }
                tokio::time::timeout(Duration::from_secs(1), worker)
                    .await
                    .unwrap()
                    .unwrap();
                refresh_idle(&tasks).await;
                drop(gate);
                drop(stream);
                assert!(rules.gate.try_write().is_ok());
                assert_eq!(storage.read(read_cache).unwrap(), saved);
                assert_eq!(rules.revision.load(Ordering::Acquire), revision);
                assert_eq!(rules.status().status.generated_at, generated_at);
                assert!(listener.accept().now_or_never().is_none());
                tasks.try_close_idle().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn periodic_rules_failed_storage_does_not_publish_verified_provider_rules() {
        let (storage, rules, listener, next) = refresh_fixture().await;
        let saved = storage.read(read_cache).unwrap();
        let generated_at = rules.status().status.generated_at;
        storage
            .transaction::<_, StorageError>(|db| {
                db.execute_batch("CREATE TRIGGER refuse_refresh BEFORE INSERT ON performance_rules BEGIN SELECT RAISE(IGNORE); END;")?;
                Ok(())
            })
            .unwrap();
        let tasks = TaskOwner::new(1).unwrap();
        let (shutdown, worker) = start_refresh(&rules, &tasks);
        respond_rules(accept_refresh(&listener).await, &next).await;
        refresh_idle(&tasks).await;
        assert_eq!(storage.read(read_cache).unwrap(), saved);
        assert_eq!(rules.status().status.generated_at, generated_at);
        assert_eq!(rules.revision.load(Ordering::Acquire), 2);
        shutdown.send_replace(true);
        worker.await.unwrap();
        tasks.try_close_idle().unwrap();
    }

    #[tokio::test]
    async fn rules_import_adopts_exact_signed_bytes_and_replay_never_rolls_back_refresh() {
        use std::io::{Read, Write};
        let (storage, rules, prepared) = fixture();
        let first = commit(&rules, &prepared).await.unwrap();
        assert!(!first.already_imported && first.stored_cache_matches_import);
        assert_eq!(
            rules.status().status.last_refresh_at.as_deref(),
            Some("2001-01-01T00:00:00Z")
        );
        assert_eq!(
            storage.read(read_cache).unwrap().as_deref(),
            prepared.cache.as_deref()
        );
        assert_eq!(rules.revision.load(Ordering::Acquire), 2);
        let replay = commit(&rules, &prepared).await.unwrap();
        assert!(replay.already_imported && replay.stored_cache_matches_import);
        assert_eq!(rules.revision.load(Ordering::Acquire), 2);
        let (next, key) = signed_cache("1999-01-01T00:00:00Z");
        let next: axial_performance::RulesCacheSnapshot = serde_json::from_slice(&next).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rules", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            let body = serde_json::to_vec(&next.manifest).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {}\r\nConnection: close\r\n\r\n", body.len(), next.signature.signature).unwrap();
            stream.write_all(&body).unwrap();
        });
        // Fresh construction proves persisted cache restoration before refresh.
        let reopened =
            PerformanceRules::with_remote(storage.clone(), Some(url), Some(key)).unwrap();
        assert_eq!(
            reopened.status().status.last_refresh_at,
            rules.status().status.last_refresh_at
        );
        reopened.refresh().await.unwrap();
        server.join().unwrap();
        let refreshed = storage.read(read_cache).unwrap().unwrap();
        assert_ne!(Some(refreshed.as_slice()), prepared.cache.as_deref());
        let revision = reopened.revision.load(Ordering::Acquire);
        let replay = commit(&reopened, &prepared).await.unwrap();
        assert!(replay.already_imported && !replay.stored_cache_matches_import);
        assert_eq!(replay.receipt, first.receipt);
        assert_eq!(storage.read(read_cache).unwrap(), Some(refreshed));
        assert_eq!(reopened.revision.load(Ordering::Acquire), revision);
        let unconfigured = PerformanceRules::with_remote(storage, None, None).unwrap();
        assert!(
            commit(&unconfigured, &prepared)
                .await
                .unwrap()
                .already_imported,
            "historical replay is not a new trust decision"
        );
    }

    #[tokio::test]
    async fn rules_import_checks_policy_collisions_and_rejected_startup_without_mutation() {
        let (storage, rules, prepared) = fixture();
        for (url, key) in [
            (None, None),
            (Some("https://example.invalid".into()), None),
            (
                Some("https://example.invalid".into()),
                Some("00".repeat(32)),
            ),
        ] {
            let other = PerformanceRules::with_remote(storage.clone(), url, key).unwrap();
            assert!(commit(&other, &prepared).await.is_err());
            assert!(storage.read(read_cache).unwrap().is_none());
            assert!(
                other
                    .import_status(&prepared.receipt.rules_import_id)
                    .unwrap()
                    .0
                    .is_none()
            );
        }
        commit(&rules, &prepared).await.unwrap();
        let (different, _) = signed_cache("2002-01-01T00:00:00Z");
        for input in [
            prepare_import(
                &"3".repeat(64),
                &"4".repeat(64),
                Some(different),
                Vec::new(),
            )
            .unwrap(),
            prepare_import(
                &prepared.source_id,
                &"5".repeat(64),
                prepared.cache.as_deref().map(Vec::from),
                Vec::new(),
            )
            .unwrap(),
        ] {
            assert!(matches!(
                commit(&rules, &input).await,
                Err(RulesImportError::Conflict)
            ));
        }
        let exact = prepare_import(
            &"6".repeat(64),
            &"7".repeat(64),
            prepared.cache.as_deref().map(Vec::from),
            Vec::new(),
        )
        .unwrap();
        assert!(!commit(&rules, &exact).await.unwrap().already_imported);
        assert_eq!(
            rules.revision.load(Ordering::Acquire),
            2,
            "exact cache is a no-op"
        );
        let rejected = PerformanceRules::with_remote(storage.clone(), None, None).unwrap();
        assert!(matches!(
            commit(
                &rejected,
                &prepare_import(
                    &"8".repeat(64),
                    &"9".repeat(64),
                    prepared.cache.as_deref().map(Vec::from),
                    Vec::new()
                )
                .unwrap()
            )
            .await,
            Err(RulesImportError::Unavailable)
        ));
        assert_eq!(
            storage.read(read_cache).unwrap().as_deref(),
            prepared.cache.as_deref()
        );
    }

    #[tokio::test]
    async fn rules_import_ignored_writes_and_failed_commit_leave_active_memory_unchanged() {
        for failure in [
            "cache",
            "receipt",
            "delete-cache",
            "delete-receipt",
            "commit",
        ] {
            let (storage, rules, prepared) = fixture();
            storage.transaction(|tx| -> Result<_, StorageError> {
                match failure {
                    "cache" => tx.execute_batch("CREATE TRIGGER ignore_import BEFORE INSERT ON performance_rules BEGIN SELECT RAISE(IGNORE); END;")?,
                    "receipt" => tx.execute_batch("CREATE TRIGGER ignore_import BEFORE INSERT ON performance_rules_imports BEGIN SELECT RAISE(IGNORE); END;")?,
                    "delete-cache" => tx.execute_batch("CREATE TRIGGER delete_import AFTER INSERT ON performance_rules BEGIN DELETE FROM performance_rules; END;")?,
                    "delete-receipt" => tx.execute_batch("CREATE TRIGGER delete_import AFTER INSERT ON performance_rules_imports BEGIN DELETE FROM performance_rules_imports; END;")?,
                    _ => { tx.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER invalid_commit AFTER INSERT ON performance_rules_imports BEGIN INSERT INTO child VALUES(1); END;")?; }
                }
                Ok(())
            }).unwrap();
            assert!(commit(&rules, &prepared).await.is_err(), "{failure}");
            assert!(storage.read(read_cache).unwrap().is_none());
            assert!(
                rules
                    .import_status(&prepared.receipt.rules_import_id)
                    .unwrap()
                    .0
                    .is_none()
            );
            assert_eq!(rules.status().status.rule_source, RuleSource::BuiltIn);
            assert_eq!(rules.revision.load(Ordering::Acquire), 1);
        }
    }

    #[tokio::test]
    async fn rules_import_waits_for_leases_then_checks_source_and_cancellation() {
        let (storage, rules, prepared) = fixture();
        let lease = rules.gate.read().await;
        let cancel = CancellationToken::new();
        let called = std::sync::atomic::AtomicBool::new(false);
        let future = rules.commit_import(&prepared, &cancel, async {
            called.store(true, Ordering::Release);
            Ok::<_, RulesImportError>(())
        });
        tokio::pin!(future);
        tokio::select! { result = &mut future => panic!("unexpected completion: {}", result.is_ok()), _ = tokio::task::yield_now() => () }
        assert!(!called.load(Ordering::Acquire));
        cancel.cancel();
        assert!(matches!(future.await, Err(RulesImportError::Cancelled)));
        drop(lease);
        assert!(matches!(
            rules
                .commit_import(&prepared, &CancellationToken::new(), async {
                    Err::<(), _>(RulesImportError::Conflict)
                })
                .await,
            Err(RulesImportError::Conflict)
        ));
        assert!(storage.read(read_cache).unwrap().is_none());
        let cancelled = CancellationToken::new();
        assert!(matches!(
            rules
                .commit_import(&prepared, &cancelled, async {
                    cancelled.cancel();
                    Ok::<_, RulesImportError>(())
                })
                .await,
            Err(RulesImportError::Cancelled)
        ));
        assert!(storage.read(read_cache).unwrap().is_none());
    }

    #[tokio::test]
    async fn rules_import_history_is_bounded_source_bound_and_never_fabricates_cache() {
        let (storage, _, _) = fixture();
        let rules = PerformanceRules::with_remote(storage.clone(), None, None).unwrap();
        let history: Vec<_> = (1..=128)
            .map(|sequence| HistoricalRulesRefresh {
                operation_id: format!("op-{}", uuid::Uuid::new_v4()),
                sequence,
                outcome: HistoricalRulesRefreshOutcome::Failed {
                    failure_point: HistoricalRulesRefreshFailure::RemoteRules,
                },
            })
            .collect();
        let prepared =
            prepare_import(&"1".repeat(64), &"2".repeat(64), None, history.clone()).unwrap();
        let result = commit(&rules, &prepared).await.unwrap();
        assert!(!result.stored_cache_matches_import);
        assert_eq!(result.receipt.refresh_history, history);
        assert!(storage.read(read_cache).unwrap().is_none());
        let proof = rules
            .completed_import(&prepared.source_id, &prepared.receipt.fingerprint)
            .unwrap()
            .unwrap();
        storage.transaction(|tx| proof.verify_in(tx)).unwrap();
        let mut excess = history;
        excess.push(HistoricalRulesRefresh {
            operation_id: format!("op-{}", uuid::Uuid::new_v4()),
            sequence: 129,
            outcome: HistoricalRulesRefreshOutcome::Succeeded {
                cache_changed: false,
            },
        });
        assert!(prepare_import(&"1".repeat(64), &"2".repeat(64), None, excess).is_err());
        storage
            .transaction(|tx| -> Result<_, StorageError> {
                tx.execute(
                    "UPDATE performance_rules_imports SET fingerprint=?1",
                    ["3".repeat(64)],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            rules
                .import_status(&prepared.receipt.rules_import_id)
                .is_err()
        );
        assert!(storage.transaction(|tx| proof.verify_in(tx)).is_err());
    }

    #[test]
    fn rules_import_sequence_wire_requires_canonical_lossless_decimal_text() {
        let record = HistoricalRulesRefresh {
            operation_id: format!("op-{}", uuid::Uuid::new_v4()),
            sequence: u64::MAX - 1,
            outcome: HistoricalRulesRefreshOutcome::Succeeded {
                cache_changed: false,
            },
        };
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["sequence"], "18446744073709551614");
        assert_eq!(
            serde_json::from_value::<HistoricalRulesRefresh>(encoded.clone()).unwrap(),
            record
        );
        for invalid in [
            serde_json::json!(9007199254740993_u64),
            serde_json::json!(null),
            serde_json::json!("0"),
            serde_json::json!("01"),
            serde_json::json!("+1"),
            serde_json::json!(" 1"),
            serde_json::json!("1.0"),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut value = encoded.clone();
            value["sequence"] = invalid;
            assert!(serde_json::from_value::<HistoricalRulesRefresh>(value).is_err());
        }
    }
}
