use crate::execution::persistence::{
    AcceptedWrite, AtomicSnapshotWriter, PersistenceCoordinator, PersistenceError,
    PersistenceOwnerLease, WriteUrgency,
};
use crate::observability::telemetry::{TelemetryHub, configured_posthog_environment};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use axial_config::{FEATURE_FLAGS, FeatureFlagDef};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const REMOTE_FLAGS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const REMOTE_FLAGS_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_FLAGS_MAX_BYTES: usize = 1024 * 1024;
const REMOTE_FLAGS_USER_AGENT: &str = concat!("axial/", env!("CARGO_PKG_VERSION"), " remote-flags");
const REMOTE_FLAGS_CACHE_FILE: &str = "remote-cache.json";
const REMOTE_FLAGS_CACHE_SCHEMA: &str = "axial.remote_flags";
const REMOTE_FLAGS_CACHE_SCHEMA_VERSION: u32 = 1;
const REMOTE_FLAG_STORE_LOCK_INVARIANT: &str =
    "remote flag store lock poisoned; committed and persisted state may diverge";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedFlag {
    pub enabled: bool,
    pub source: ResolvedFlagSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedFlagSource {
    Default,
    Override,
    Remote,
}

pub(crate) fn resolve_flag(
    flag: &FeatureFlagDef,
    overrides: &BTreeMap<String, bool>,
    remote_active: bool,
    remote_values: &BTreeMap<String, bool>,
) -> ResolvedFlag {
    if let Some(enabled) = overrides.get(flag.key).copied() {
        return ResolvedFlag {
            enabled,
            source: ResolvedFlagSource::Override,
        };
    }

    if remote_active
        && !flag.dev_only
        && let Some(enabled) = remote_values.get(flag.key).copied()
    {
        return ResolvedFlag {
            enabled,
            source: ResolvedFlagSource::Remote,
        };
    }

    ResolvedFlag {
        enabled: flag.default_enabled,
        source: ResolvedFlagSource::Default,
    }
}

pub(crate) struct RemoteFlagStore {
    state: Arc<Mutex<RemoteFlagState>>,
    mutation_gate: Arc<AsyncMutex<()>>,
    refresh_gate: AsyncMutex<()>,
    persistence: Option<RemoteFlagPersistence>,
}

struct RemoteFlagState {
    visible: Option<RemoteFlagCacheSnapshot>,
    retry_candidate: Option<(u64, RemoteFlagCacheSnapshot)>,
}

struct RemoteFlagPersistence {
    owner: PersistenceOwnerLease,
    writer: AtomicSnapshotWriter,
}

impl RemoteFlagPersistence {
    fn claim_with_coordinator(
        cache_path: &Path,
        coordinator: PersistenceCoordinator,
    ) -> io::Result<Self> {
        let owner = coordinator
            .claim_owner(cache_path)
            .map_err(io::Error::from)?;
        let writer = owner
            .writer(cache_path, remote_flags_cache_target())
            .map_err(io::Error::from)?;
        Ok(Self { owner, writer })
    }
}

struct PendingRemoteFlagCommit {
    ticket: AcceptedWrite,
    revision: u64,
    candidate: RemoteFlagCacheSnapshot,
}

impl RemoteFlagStore {
    pub(crate) async fn load_from_config_dir(config_dir: PathBuf) -> Self {
        tokio::task::spawn_blocking(move || {
            Self::load_from_config_dir_blocking(
                config_dir,
                PersistenceCoordinator::global(),
                Utc::now(),
            )
        })
        .await
        .unwrap_or_else(|_| panic!("remote flag startup task stopped"))
        .unwrap_or_else(|_| panic!("failed to initialize remote flag persistence"))
    }

    fn load_from_config_dir_blocking(
        config_dir: PathBuf,
        coordinator: PersistenceCoordinator,
        now: DateTime<Utc>,
    ) -> io::Result<Self> {
        let cache_path = remote_flags_cache_path(&config_dir);
        let persistence = RemoteFlagPersistence::claim_with_coordinator(&cache_path, coordinator)?;
        let visible = load_remote_flags_cache_with_registry(&cache_path, now, FEATURE_FLAGS);

        Ok(Self::from_parts(visible, Some(persistence)))
    }

    fn from_parts(
        visible: Option<RemoteFlagCacheSnapshot>,
        persistence: Option<RemoteFlagPersistence>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RemoteFlagState {
                visible,
                retry_candidate: None,
            })),
            mutation_gate: Arc::new(AsyncMutex::new(())),
            refresh_gate: AsyncMutex::new(()),
            persistence,
        }
    }

    pub(crate) fn values_snapshot(&self) -> BTreeMap<String, bool> {
        self.state
            .lock()
            .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
            .visible
            .as_ref()
            .map(|snapshot| snapshot.values.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn fetched_at(&self) -> Option<String> {
        self.state
            .lock()
            .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
            .visible
            .as_ref()
            .map(|snapshot| snapshot.fetched_at.clone())
    }

    pub(crate) async fn refresh_once(
        &self,
        telemetry: &TelemetryHub,
    ) -> Result<RemoteFlagRefreshOutcome, RemoteFlagRefreshError> {
        let _refresh = self.refresh_gate.lock().await;
        let Some(key) = telemetry.configured_posthog_key() else {
            return Ok(RemoteFlagRefreshOutcome::Skipped);
        };
        let Some(distinct_id) = telemetry.current_telemetry_install_id() else {
            return Ok(RemoteFlagRefreshOutcome::Skipped);
        };
        self.reconcile_retry().await?;
        let request = RemoteFlagFetchRequest {
            host: telemetry.configured_posthog_host(),
            key,
            distinct_id,
        };
        let values = fetch_remote_flags(&request).await?;
        let flag_count = values.len();
        let snapshot = RemoteFlagCacheSnapshot {
            schema: REMOTE_FLAGS_CACHE_SCHEMA.to_string(),
            schema_version: REMOTE_FLAGS_CACHE_SCHEMA_VERSION,
            fetched_at: Utc::now().to_rfc3339(),
            values,
        };

        self.commit(snapshot).await?;

        Ok(RemoteFlagRefreshOutcome::Refreshed { flag_count })
    }

    pub(crate) async fn close(&self) -> Result<(), RemoteFlagRefreshError> {
        let _refresh = self.refresh_gate.lock().await;
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        if let Some(persistence) = &self.persistence {
            persistence.owner.close().await?;
        }
        drop(mutation);
        Ok(())
    }

    async fn reconcile_retry(&self) -> Result<(), RemoteFlagRefreshError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        drop(mutation);
        Ok(())
    }

    async fn reconcile_retry_holding_gate(
        &self,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, RemoteFlagRefreshError> {
        let retained = self
            .state
            .lock()
            .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
            .retry_candidate
            .clone();
        let Some((candidate_revision, candidate)) = retained else {
            return Ok(mutation);
        };
        let Some(persistence) = &self.persistence else {
            self.state
                .lock()
                .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
                .retry_candidate = None;
            return Ok(mutation);
        };
        let ticket = persistence.writer.retry()?;
        let revision = ticket.revision().get();
        assert_eq!(
            revision, candidate_revision,
            "remote flag retry revision diverged from its exact candidate"
        );
        self.await_commit_holding_gate(
            PendingRemoteFlagCommit {
                ticket,
                revision,
                candidate,
            },
            mutation,
        )
        .await
    }

    async fn commit(
        &self,
        candidate: RemoteFlagCacheSnapshot,
    ) -> Result<(), RemoteFlagRefreshError> {
        let mutation = self.mutation_gate.clone().lock_owned().await;
        let mutation = self.reconcile_retry_holding_gate(mutation).await?;
        let Some(persistence) = &self.persistence else {
            self.state
                .lock()
                .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
                .visible = Some(candidate);
            drop(mutation);
            return Ok(());
        };
        let ticket = persistence.writer.accept(
            candidate.clone(),
            WriteUrgency::Immediate,
            encode_remote_flags_cache,
        )?;
        let revision = ticket.revision().get();
        let mutation = self
            .await_commit_holding_gate(
                PendingRemoteFlagCommit {
                    ticket,
                    revision,
                    candidate,
                },
                mutation,
            )
            .await?;
        drop(mutation);
        Ok(())
    }

    async fn await_commit_holding_gate(
        &self,
        commit: PendingRemoteFlagCommit,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<OwnedMutexGuard<()>, RemoteFlagRefreshError> {
        let state = self.state.clone();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        commit.ticket.observe(move |result| {
            let result: Result<(), RemoteFlagRefreshError> = match result {
                Ok(_) => {
                    let mut state = state.lock().expect(REMOTE_FLAG_STORE_LOCK_INVARIANT);
                    state.visible = Some(commit.candidate);
                    if state
                        .retry_candidate
                        .as_ref()
                        .is_some_and(|(revision, _)| *revision == commit.revision)
                    {
                        state.retry_candidate = None;
                    }
                    Ok(())
                }
                Err(error) => {
                    state
                        .lock()
                        .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
                        .retry_candidate = Some((commit.revision, commit.candidate));
                    Err(error.into())
                }
            };
            let _ = completed_tx.send((result, mutation));
        });
        let (result, mutation) = completed_rx
            .await
            .map_err(|_| RemoteFlagRefreshError::CommitObserverStopped)?;
        result?;
        Ok(mutation)
    }

    #[cfg(test)]
    pub(crate) fn replace_values_for_test(
        &self,
        values: BTreeMap<String, bool>,
        fetched_at: Option<String>,
    ) {
        let visible = fetched_at.map(|fetched_at| RemoteFlagCacheSnapshot {
            schema: REMOTE_FLAGS_CACHE_SCHEMA.to_string(),
            schema_version: REMOTE_FLAGS_CACHE_SCHEMA_VERSION,
            fetched_at,
            values,
        });
        self.state
            .lock()
            .expect(REMOTE_FLAG_STORE_LOCK_INVARIANT)
            .visible = visible;
    }
}

#[cfg(test)]
impl Default for RemoteFlagStore {
    fn default() -> Self {
        Self::from_parts(None, None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteFlagRefreshOutcome {
    Refreshed { flag_count: usize },
    Skipped,
}

#[derive(Debug, Error)]
pub(crate) enum RemoteFlagRefreshError {
    #[error("request failed")]
    Request(#[from] reqwest::Error),
    #[error("http status {0}")]
    HttpStatus(u16),
    #[error("response too large")]
    ResponseTooLarge,
    #[error("response parse failed")]
    Parse(#[from] serde_json::Error),
    #[error("cache persistence failed")]
    Persistence,
    #[error("cache commit observer stopped")]
    CommitObserverStopped,
}

impl From<PersistenceError> for RemoteFlagRefreshError {
    fn from(_: PersistenceError) -> Self {
        Self::Persistence
    }
}

#[derive(Debug, Clone)]
struct RemoteFlagFetchRequest {
    host: String,
    key: String,
    distinct_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteFlagCacheSnapshot {
    schema: String,
    schema_version: u32,
    fetched_at: String,
    values: BTreeMap<String, bool>,
}

#[derive(Debug, Deserialize)]
struct RemoteFlagsEnvelope {
    #[serde(default)]
    flags: BTreeMap<String, RemoteFlagEvaluation>,
    #[serde(default, rename = "errorsWhileComputingFlags")]
    _errors_while_computing_flags: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteFlagEvaluation {
    enabled: Option<bool>,
}

fn remote_flags_cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("flags").join(REMOTE_FLAGS_CACHE_FILE)
}

async fn fetch_remote_flags(
    request: &RemoteFlagFetchRequest,
) -> Result<BTreeMap<String, bool>, RemoteFlagRefreshError> {
    let url = format!("{}/flags?v=2", request.host.trim_end_matches('/'));
    let response = remote_flags_client()
        .post(url)
        .json(&serde_json::json!({
            "api_key": request.key.as_str(),
            "distinct_id": request.distinct_id.as_str(),
            "properties": {
                "environment": configured_posthog_environment(),
            },
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(RemoteFlagRefreshError::HttpStatus(
            response.status().as_u16(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > REMOTE_FLAGS_MAX_BYTES as u64)
    {
        return Err(RemoteFlagRefreshError::ResponseTooLarge);
    }

    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > REMOTE_FLAGS_MAX_BYTES {
            return Err(RemoteFlagRefreshError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    let values = parse_remote_flag_response_values(&body)?;
    Ok(filter_registered_remote_values_with_registry(
        values,
        FEATURE_FLAGS,
    ))
}

fn parse_remote_flag_response_values(
    body: &[u8],
) -> Result<BTreeMap<String, bool>, serde_json::Error> {
    let envelope = serde_json::from_slice::<RemoteFlagsEnvelope>(body)?;
    Ok(envelope
        .flags
        .into_iter()
        .filter_map(|(key, evaluation)| evaluation.enabled.map(|enabled| (key, enabled)))
        .collect())
}

fn load_remote_flags_cache_with_registry(
    path: &Path,
    now: DateTime<Utc>,
    registry: &[FeatureFlagDef],
) -> Option<RemoteFlagCacheSnapshot> {
    let mut file = File::open(path).ok()?;
    let mut data = Vec::new();
    file.by_ref()
        .take((REMOTE_FLAGS_MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() > REMOTE_FLAGS_MAX_BYTES {
        return None;
    }
    let mut snapshot = serde_json::from_slice::<RemoteFlagCacheSnapshot>(&data).ok()?;
    if snapshot.schema != REMOTE_FLAGS_CACHE_SCHEMA
        || snapshot.schema_version != REMOTE_FLAGS_CACHE_SCHEMA_VERSION
    {
        return None;
    }
    let fetched_at = DateTime::parse_from_rfc3339(&snapshot.fetched_at)
        .ok()?
        .with_timezone(&Utc);
    let age = now.signed_duration_since(fetched_at).to_std().ok()?;
    if age > REMOTE_FLAGS_CACHE_TTL {
        return None;
    }

    snapshot.values = filter_registered_remote_values_with_registry(snapshot.values, registry);
    Some(snapshot)
}

fn encode_remote_flags_cache(snapshot: RemoteFlagCacheSnapshot) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn filter_registered_remote_values_with_registry(
    values: BTreeMap<String, bool>,
    registry: &[FeatureFlagDef],
) -> BTreeMap<String, bool> {
    values
        .into_iter()
        .filter(|(key, _)| {
            registry
                .iter()
                .any(|flag| flag.key == key.as_str() && !flag.dev_only)
        })
        .collect()
}

fn remote_flags_cache_target() -> TargetDescriptor {
    TargetDescriptor::new(
        StabilizationSystem::State,
        TargetKind::Config,
        "remote_feature_flags",
        OwnershipClass::LauncherManaged,
    )
}

fn remote_flags_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(REMOTE_FLAGS_USER_AGENT)
                .timeout(REMOTE_FLAGS_HTTP_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::persistence::AtomicWriteBackend;
    use axial_config::{AppConfig, FlagStage};
    use axum::{Json, Router, extract::State, http::StatusCode, http::Uri, routing::post};
    use serde_json::Value;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{Notify, mpsc};

    const TEST_KEY: &str = "remote.test";
    const DEV_KEY: &str = "remote.dev-only";
    const POSTHOG_KEY: &str = "phc_test";
    const INSTALL_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
    static TEST_REGISTRY: &[FeatureFlagDef] = &[
        FeatureFlagDef {
            key: TEST_KEY,
            title: "Remote test",
            description: "Remote test flag",
            stage: FlagStage::Beta,
            dev_only: false,
            default_enabled: true,
        },
        FeatureFlagDef {
            key: DEV_KEY,
            title: "Remote dev test",
            description: "Remote dev flag",
            stage: FlagStage::Experimental,
            dev_only: true,
            default_enabled: false,
        },
    ];

    struct RecordingBackend {
        attempts: AtomicUsize,
        failures: AtomicUsize,
        committed: Mutex<Vec<Vec<u8>>>,
        started: Notify,
        gate: Mutex<Option<Arc<WriteGate>>>,
    }

    struct WriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    struct WriteGateHandle(Arc<WriteGate>);

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
                failures: AtomicUsize::new(0),
                committed: Mutex::new(Vec::new()),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }

        fn gate_next(&self) -> WriteGateHandle {
            let gate = Arc::new(WriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("backend gate lock") = Some(gate.clone());
            WriteGateHandle(gate)
        }

        async fn wait_for_attempt(&self, expected: usize) {
            loop {
                let started = self.started.notified();
                if self.attempts.load(Ordering::SeqCst) >= expected {
                    return;
                }
                started.await;
            }
        }

        fn committed_snapshots(&self) -> Vec<RemoteFlagCacheSnapshot> {
            self.committed
                .lock()
                .expect("committed cache lock")
                .iter()
                .map(|contents| {
                    serde_json::from_slice(contents).expect("decode committed remote flag cache")
                })
                .collect()
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            _target: &TargetDescriptor,
            _destination: &Path,
            contents: &[u8],
        ) -> io::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(gate) = self.gate.lock().expect("backend gate lock").take() {
                gate.wait();
            }
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok()
            {
                return Err(io::Error::other("injected remote flag cache failure"));
            }
            self.committed
                .lock()
                .expect("committed cache lock")
                .push(contents.to_vec());
            Ok(())
        }
    }

    impl WriteGate {
        fn release(&self) {
            *self.released.lock().expect("write gate lock") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("write gate lock");
            while !*released {
                released = self.changed.wait(released).expect("wait on write gate");
            }
        }
    }

    impl WriteGateHandle {
        fn release(&self) {
            self.0.release();
        }
    }

    impl Drop for WriteGateHandle {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    #[test]
    fn resolver_prefers_override_then_remote_then_default_and_reports_source() {
        let flag = test_flag(false);
        let remote_values = BTreeMap::from([(TEST_KEY.to_string(), true)]);
        let overrides = BTreeMap::new();

        let remote = resolve_flag(&flag, &overrides, true, &remote_values);
        assert!(remote.enabled);
        assert_eq!(remote.source, ResolvedFlagSource::Remote);

        let default = resolve_flag(&flag, &overrides, false, &remote_values);
        assert!(!default.enabled);
        assert_eq!(default.source, ResolvedFlagSource::Default);

        let overrides = BTreeMap::from([(TEST_KEY.to_string(), false)]);
        let override_resolution = resolve_flag(&flag, &overrides, true, &remote_values);
        assert!(!override_resolution.enabled);
        assert_eq!(override_resolution.source, ResolvedFlagSource::Override);
    }

    #[test]
    fn resolver_distinguishes_remote_false_from_absent() {
        let flag = test_flag(true);
        let remote_values = BTreeMap::from([(TEST_KEY.to_string(), false)]);

        let resolution = resolve_flag(&flag, &BTreeMap::new(), true, &remote_values);

        assert!(!resolution.enabled);
        assert_eq!(resolution.source, ResolvedFlagSource::Remote);
    }

    #[test]
    fn resolver_ignores_remote_values_for_dev_only_flags() {
        let flag = FeatureFlagDef {
            key: DEV_KEY,
            title: "Remote dev test",
            description: "Remote dev flag",
            stage: FlagStage::Experimental,
            dev_only: true,
            default_enabled: false,
        };
        let remote_values = BTreeMap::from([(DEV_KEY.to_string(), true)]);

        let resolution = resolve_flag(&flag, &BTreeMap::new(), true, &remote_values);

        assert!(!resolution.enabled);
        assert_eq!(resolution.source, ResolvedFlagSource::Default);
    }

    #[test]
    fn remote_values_for_unknown_and_dev_only_keys_are_ignored() {
        let values = BTreeMap::from([
            (TEST_KEY.to_string(), true),
            (DEV_KEY.to_string(), true),
            ("unknown.flag".to_string(), true),
        ]);

        let filtered = filter_registered_remote_values_with_registry(values, TEST_REGISTRY);

        assert_eq!(filtered, BTreeMap::from([(TEST_KEY.to_string(), true)]));
    }

    #[test]
    fn cache_load_respects_ttl_and_filters_values() {
        let root = test_root("cache-ttl");
        let path = remote_flags_cache_path(&root);
        let now = Utc::now();
        let snapshot = cache_snapshot(
            (now - chrono::Duration::hours(1)).to_rfc3339(),
            BTreeMap::from([
                (TEST_KEY.to_string(), false),
                (DEV_KEY.to_string(), true),
                ("unknown.flag".to_string(), true),
            ]),
        );
        seed_cache(&path, &snapshot);

        let loaded =
            load_remote_flags_cache_with_registry(&path, now, TEST_REGISTRY).expect("fresh cache");

        assert_eq!(
            loaded.values,
            BTreeMap::from([(TEST_KEY.to_string(), false)])
        );
        assert_eq!(loaded.fetched_at, snapshot.fetched_at);

        let stale = cache_snapshot(
            (now - chrono::Duration::hours(25)).to_rfc3339(),
            BTreeMap::from([(TEST_KEY.to_string(), true)]),
        );
        seed_cache(&path, &stale);

        assert!(load_remote_flags_cache_with_registry(&path, now, TEST_REGISTRY).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_rejects_unknown_fields_and_unparseable_timestamps() {
        let root = test_root("cache-junk");
        let path = remote_flags_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema": REMOTE_FLAGS_CACHE_SCHEMA,
                "schema_version": REMOTE_FLAGS_CACHE_SCHEMA_VERSION,
                "fetched_at": Utc::now().to_rfc3339(),
                "values": { "remote.test": true },
                "junk": true
            }))
            .expect("serialize junk cache"),
        )
        .expect("write junk cache");
        assert!(load_remote_flags_cache_with_registry(&path, Utc::now(), TEST_REGISTRY).is_none());

        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema": REMOTE_FLAGS_CACHE_SCHEMA,
                "schema_version": REMOTE_FLAGS_CACHE_SCHEMA_VERSION,
                "fetched_at": "not a timestamp",
                "values": { "remote.test": true }
            }))
            .expect("serialize bad timestamp cache"),
        )
        .expect("write bad timestamp cache");
        assert!(load_remote_flags_cache_with_registry(&path, Utc::now(), TEST_REGISTRY).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_encoder_emits_only_the_strict_current_schema() {
        let snapshot = cache_snapshot(
            "2026-01-02T00:00:00Z".to_string(),
            BTreeMap::from([(TEST_KEY.to_string(), false)]),
        );

        let raw = encode_remote_flags_cache(snapshot).expect("encode cache");
        let value = serde_json::from_slice::<Value>(&raw).expect("cache json");
        let object = value.as_object().expect("cache object");
        assert_eq!(object.len(), 4);
        assert_eq!(value["schema"], REMOTE_FLAGS_CACHE_SCHEMA);
        assert_eq!(value["schema_version"], REMOTE_FLAGS_CACHE_SCHEMA_VERSION);
        assert!(object.contains_key("fetched_at"));
        assert!(object.contains_key("values"));
        assert_eq!(value["values"][TEST_KEY], false);
    }

    #[test]
    fn remote_flag_cache_target_is_launcher_managed() {
        let target = remote_flags_cache_target();

        assert_eq!(target.system, StabilizationSystem::State);
        assert_eq!(target.kind, TargetKind::Config);
        assert_eq!(target.ownership, OwnershipClass::LauncherManaged);
    }

    #[test]
    fn store_tracks_values_and_fetch_timestamp() {
        let store = RemoteFlagStore::default();
        let fetched_at = "2026-01-02T00:00:00Z".to_string();

        store.replace_values_for_test(
            BTreeMap::from([(TEST_KEY.to_string(), false)]),
            Some(fetched_at.clone()),
        );

        assert_eq!(
            store.values_snapshot(),
            BTreeMap::from([(TEST_KEY.to_string(), false)])
        );
        assert_eq!(store.fetched_at(), Some(fetched_at));
    }

    #[tokio::test]
    async fn cancelled_cache_commit_stays_hidden_then_publishes_exact_snapshot() {
        let (root, backend, store) = persistence_fixture("cancelled-commit");
        let store = Arc::new(store);
        let candidate = cache_snapshot(
            "2026-01-02T00:00:00Z".to_string(),
            BTreeMap::from([(TEST_KEY.to_string(), false)]),
        );
        let gate = backend.gate_next();
        let task_store = store.clone();
        let task_candidate = candidate.clone();
        let task = tokio::spawn(async move { task_store.commit(task_candidate).await });

        backend.wait_for_attempt(1).await;
        assert!(store.values_snapshot().is_empty());
        task.abort();
        assert!(task.await.expect_err("caller is cancelled").is_cancelled());
        gate.release();
        store.close().await.expect("observer settles before close");

        assert_eq!(backend.committed_snapshots(), vec![candidate.clone()]);
        assert_eq!(store.values_snapshot(), candidate.values);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn later_cache_commit_retries_exact_failed_snapshot_before_new_candidate() {
        let (root, backend, store) = persistence_fixture("exact-retry");
        let first = cache_snapshot(
            "2026-01-02T00:00:00Z".to_string(),
            BTreeMap::from([(TEST_KEY.to_string(), true)]),
        );
        let second = cache_snapshot(
            "2026-01-03T00:00:00Z".to_string(),
            BTreeMap::from([(TEST_KEY.to_string(), false)]),
        );
        backend.fail_next();

        assert!(matches!(
            store.commit(first.clone()).await,
            Err(RemoteFlagRefreshError::Persistence)
        ));
        assert!(store.values_snapshot().is_empty());
        store
            .commit(second.clone())
            .await
            .expect("retry first then commit second");

        assert_eq!(backend.committed_snapshots(), vec![first, second.clone()]);
        assert_eq!(store.values_snapshot(), second.values);
        store.close().await.expect("close remote flag store");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn close_retries_exact_failed_cache_and_rejects_later_commits() {
        let (root, backend, store) = persistence_fixture("close-retry");
        let candidate = cache_snapshot(
            "2026-01-02T00:00:00Z".to_string(),
            BTreeMap::from([(TEST_KEY.to_string(), true)]),
        );
        backend.fail_next();
        assert!(matches!(
            store.commit(candidate.clone()).await,
            Err(RemoteFlagRefreshError::Persistence)
        ));

        store.close().await.expect("close retries exact cache");
        store.close().await.expect("close is idempotent");

        assert_eq!(backend.committed_snapshots(), vec![candidate.clone()]);
        assert_eq!(store.values_snapshot(), candidate.values);
        assert!(matches!(
            store
                .commit(cache_snapshot(
                    "2026-01-03T00:00:00Z".to_string(),
                    BTreeMap::from([(TEST_KEY.to_string(), false)]),
                ))
                .await,
            Err(RemoteFlagRefreshError::Persistence)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hostile_cache_is_rejected_without_rewrite() {
        let root = test_root("hostile-cache");
        let path = remote_flags_cache_path(&root);
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        fs::write(&path, br#"{"schema":"foreign","secret":"preserve"}"#)
            .expect("seed hostile cache");
        let original = fs::read(&path).expect("read hostile cache");
        let backend = Arc::new(RecordingBackend::new());
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);

        let store =
            RemoteFlagStore::load_from_config_dir_blocking(root.clone(), coordinator, Utc::now())
                .expect("load remote flag store");

        assert!(store.values_snapshot().is_empty());
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(&path).expect("reread hostile cache"), original);
        store.close().await.expect("close remote flag store");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn response_parser_tolerates_v2_extra_fields_and_preserves_enabled_false() {
        let body = serde_json::to_vec(&serde_json::json!({
            "flags": {
                "remote.test": {
                    "key": TEST_KEY,
                    "enabled": false,
                    "variant": "ignored",
                    "reason": { "code": "condition_match" }
                },
                "missing.enabled": {
                    "key": "missing.enabled"
                }
            },
            "errorsWhileComputingFlags": true,
            "extra": "ignored"
        }))
        .expect("serialize response");

        let values = parse_remote_flag_response_values(&body).expect("parse response");

        assert_eq!(values.get(TEST_KEY), Some(&false));
        assert!(!values.contains_key("missing.enabled"));
    }

    #[tokio::test]
    async fn fetch_posts_posthog_flags_v2_body_shape() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping socket remote flag fetch test: bind denied");
                return;
            }
            Err(error) => panic!("bind remote flag test server: {error}"),
        };
        let addr = listener.local_addr().expect("test listener addr");
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Option<String>, Value)>();
        let app = Router::new()
            .route("/flags", post(capture_flags))
            .with_state(tx);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let request = RemoteFlagFetchRequest {
            host: format!("http://{addr}"),
            key: POSTHOG_KEY.to_string(),
            distinct_id: INSTALL_ID.to_string(),
        };

        let values = fetch_remote_flags(&request).await.expect("fetch flags");

        let (path, query, body) = rx.recv().await.expect("captured flags request");
        server.abort();

        assert_eq!(path, "/flags");
        assert_eq!(query.as_deref(), Some("v=2"));
        assert_eq!(body["api_key"], POSTHOG_KEY);
        assert!(body.get("token").is_none());
        assert_eq!(body["distinct_id"], INSTALL_ID);
        assert_eq!(
            body["properties"]["environment"],
            configured_posthog_environment()
        );
        assert!(values.is_empty());
    }

    #[tokio::test]
    async fn refresh_skips_when_install_id_is_empty_without_generating_one() {
        let root = test_root("empty-install-id");
        let paths = test_paths(&root);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load config store"),
        );
        config
            .replace_in_memory(AppConfig {
                telemetry_enabled: true,
                telemetry_install_id: String::new(),
                ..AppConfig::default()
            })
            .expect("seed config");
        let telemetry = TelemetryHub::new(
            config.clone(),
            Some(POSTHOG_KEY.to_string()),
            "http://127.0.0.1:9".to_string(),
        );
        let store = RemoteFlagStore::default();

        assert_eq!(
            store.refresh_once(&telemetry).await.expect("skip refresh"),
            RemoteFlagRefreshOutcome::Skipped
        );
        assert!(config.current().telemetry_install_id.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    async fn capture_flags(
        State(tx): State<mpsc::UnboundedSender<(String, Option<String>, Value)>>,
        uri: Uri,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let _ = tx.send((
            uri.path().to_string(),
            uri.query().map(str::to_string),
            body,
        ));
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "flags": {
                    "dev.state-inspector": {
                        "key": "dev.state-inspector",
                        "enabled": true
                    }
                },
                "errorsWhileComputingFlags": true
            })),
        )
    }

    fn test_flag(default_enabled: bool) -> FeatureFlagDef {
        FeatureFlagDef {
            key: TEST_KEY,
            title: "Remote test",
            description: "Remote test flag",
            stage: FlagStage::Beta,
            dev_only: false,
            default_enabled,
        }
    }

    fn cache_snapshot(
        fetched_at: String,
        values: BTreeMap<String, bool>,
    ) -> RemoteFlagCacheSnapshot {
        RemoteFlagCacheSnapshot {
            schema: REMOTE_FLAGS_CACHE_SCHEMA.to_string(),
            schema_version: REMOTE_FLAGS_CACHE_SCHEMA_VERSION,
            fetched_at,
            values,
        }
    }

    fn seed_cache(path: &Path, snapshot: &RemoteFlagCacheSnapshot) {
        fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache parent");
        fs::write(
            path,
            encode_remote_flags_cache(snapshot.clone()).expect("encode cache fixture"),
        )
        .expect("write cache fixture");
    }

    fn persistence_fixture(name: &str) -> (PathBuf, Arc<RecordingBackend>, RemoteFlagStore) {
        let root = test_root(name);
        let cache_path = remote_flags_cache_path(&root);
        let backend = Arc::new(RecordingBackend::new());
        let coordinator =
            PersistenceCoordinator::for_test(backend.clone(), Duration::ZERO, Duration::ZERO);
        let persistence = RemoteFlagPersistence::claim_with_coordinator(&cache_path, coordinator)
            .expect("claim remote flag persistence");
        let store = RemoteFlagStore::from_parts(None, Some(persistence));
        (root, backend, store)
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "axial-api-remote-flags-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> axial_config::AppPaths {
        let config_dir = root.join("config");
        axial_config::AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }
}
