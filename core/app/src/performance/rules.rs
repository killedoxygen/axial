//! A rule revision remains pinned throughout an accepted managed operation.

use super::model::*;
use crate::{
    storage::{
        MetadataStore, Migration, StorageError,
        rusqlite::{Connection, OptionalExtension},
    },
    tasks::{SpawnError, TaskOwner},
};
use axial_performance::{
    CompositionPlan, PerformanceManager, PerformanceRulesAuthority, ResolutionRequest,
    RulesRefreshError,
};
use std::{
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

fn read_cache(db: &Connection) -> Result<Option<Vec<u8>>, StorageError> {
    db.query_row(
        "SELECT snapshot FROM performance_rules WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(StorageError::from)
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
        let bytes = storage.read(read_cache)?;
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

    pub async fn refresh(&self) -> Result<PerformanceRulesStatusResponse, RulesWorkflowError> {
        let _lease = self.gate.write().await;
        if !self.authority.mutation_allowed() {
            return Err(RulesWorkflowError::Changed);
        }
        let candidate = self.authority.fetch_remote_rules().await.map_err(|error| {
            let failure = match &error {
                RulesRefreshError::Unconfigured => return RulesWorkflowError::Unconfigured,
                RulesRefreshError::Request(_)
                | RulesRefreshError::HttpStatus(_)
                | RulesRefreshError::ResponseTooLarge
                | RulesRefreshError::Parse(_)
                | RulesRefreshError::Validation(_)
                | RulesRefreshError::Signature(_) => RulesWorkflowError::ProviderFailed,
                RulesRefreshError::Cache(_) => RulesWorkflowError::RefreshFailed,
            };
            self.authority
                .record_refresh_warning(axial_performance::remote_rules_refresh_warning(
                    "failed", &error,
                ));
            failure
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
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_cache(time: &str) -> (Vec<u8>, String) {
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

    async fn refresh_fixture() -> (
        Arc<MetadataStore>,
        PerformanceRules,
        tokio::net::TcpListener,
        axial_performance::RulesCacheSnapshot,
    ) {
        let storage = Arc::new(MetadataStore::in_memory().unwrap());
        storage.migrate(&[MIGRATION]).unwrap();
        let (initial, _) = signed_cache("2001-01-01T00:00:00Z");
        let initial = serde_json::from_slice(&initial).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (next, key) = signed_cache("2002-01-01T00:00:00Z");
        let rules = PerformanceRules::with_remote(
            storage.clone(),
            Some(format!("http://{}/rules", listener.local_addr().unwrap())),
            Some(key),
        )
        .unwrap();
        let (result, ()) = tokio::join!(rules.refresh(), async {
            respond_rules(accept_refresh(&listener).await, &initial).await;
        });
        result.unwrap();
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
    fn periodic_rules_interval_preserves_default_and_bounds() {
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
    async fn signed_cache_reopen_requires_current_trust_and_refresh_preserves_latest_projection() {
        let (storage, rules, listener, next) = refresh_fixture().await;
        let saved = storage.read(read_cache).unwrap();
        let url = format!("http://{}/rules", listener.local_addr().unwrap());
        for (remote, key) in [
            (None, None),
            (Some(url.clone()), None),
            (Some(url.clone()), Some("00".repeat(32))),
        ] {
            let rejected = PerformanceRules::with_remote(storage.clone(), remote, key).unwrap();
            assert_eq!(
                rejected.status().status.rules_cache.state,
                RulesCacheState::Invalid
            );
            assert!(matches!(
                rejected.refresh().await,
                Err(RulesWorkflowError::Changed)
            ));
            assert!(matches!(
                rejected
                    .plan(ResolutionRequest {
                        game_version: "1.21.1".into(),
                        loader: "fabric".into(),
                        mode: PerformanceMode::Managed,
                        hardware: Default::default(),
                        installed_mods: Vec::new(),
                    })
                    .await,
                Err(RulesWorkflowError::Changed)
            ));
            assert_eq!(storage.read(read_cache).unwrap(), saved);
        }
        let (_, key) = signed_cache("2001-01-01T00:00:00Z");
        let reopened =
            PerformanceRules::with_remote(storage.clone(), Some(url.clone()), Some(key.clone()))
                .unwrap();
        assert_eq!(reopened.status().status.rule_source, RuleSource::Remote);
        assert_eq!(
            reopened.status().status.generated_at,
            rules.status().status.generated_at
        );
        assert_eq!(
            reopened.status().status.last_refresh_at,
            rules.status().status.last_refresh_at
        );
        let (result, ()) = tokio::join!(reopened.refresh(), async {
            respond_rules(accept_refresh(&listener).await, &next).await;
        });
        let refreshed = result.unwrap();
        let saved: axial_performance::RulesCacheSnapshot =
            serde_json::from_slice(&storage.read(read_cache).unwrap().unwrap()).unwrap();
        assert_eq!(saved.manifest, next.manifest);
        assert_eq!(refreshed.status.generated_at, next.generated_at);
        assert_eq!(reopened.status().status.generated_at, next.generated_at);
        let latest = PerformanceRules::with_remote(storage, Some(url), Some(key)).unwrap();
        assert_eq!(latest.status().status.generated_at, next.generated_at);
        assert_eq!(
            latest.status().status.last_refresh_at,
            refreshed.status.last_refresh_at
        );
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
        for failure in ["ignored", "deleted", "commit"] {
            let (storage, rules, listener, next) = refresh_fixture().await;
            let saved = storage.read(read_cache).unwrap();
            let generated_at = rules.status().status.generated_at;
            storage.transaction::<_, StorageError>(|db| {
                match failure {
                    "ignored" => db.execute_batch("CREATE TRIGGER refuse_refresh BEFORE INSERT ON performance_rules BEGIN SELECT RAISE(IGNORE); END;")?,
                    "deleted" => db.execute_batch("CREATE TRIGGER delete_refresh AFTER UPDATE ON performance_rules BEGIN DELETE FROM performance_rules; END;")?,
                    _ => db.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER invalid_commit AFTER UPDATE ON performance_rules BEGIN INSERT INTO child VALUES(1); END;")?,
                }
                Ok(())
            }).unwrap();
            let tasks = TaskOwner::new(1).unwrap();
            let (shutdown, worker) = start_refresh(&rules, &tasks);
            respond_rules(accept_refresh(&listener).await, &next).await;
            refresh_idle(&tasks).await;
            assert_eq!(storage.read(read_cache).unwrap(), saved, "{failure}");
            assert_eq!(
                rules.status().status.generated_at,
                generated_at,
                "{failure}"
            );
            assert_eq!(rules.revision.load(Ordering::Acquire), 2, "{failure}");
            shutdown.send_replace(true);
            worker.await.unwrap();
            tasks.try_close_idle().unwrap();
        }
    }
}
