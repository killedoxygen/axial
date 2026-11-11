use super::*;
use ed25519_dalek::{Signer, SigningKey};

fn settings(url: Option<String>, public_key: Option<String>) -> RulesSettings {
    RulesSettings::new(url.as_deref(), public_key.as_deref(), None)
}

fn signed_cache(time: &str) -> (Vec<u8>, String) {
    let key = SigningKey::from_bytes(&[23; 32]);
    let mut manifest = crate::performance::resolve::builtin_manifest().unwrap();
    manifest.generated_at = time.into();
    let signature = key.sign(&canonical_manifest_payload(&manifest).unwrap());
    let snapshot = RulesCacheSnapshot {
        rule_source: RuleSource::Remote,
        rule_channel: RuleChannel::Remote,
        schema_version: manifest.schema_version,
        generated_at: time.into(),
        validation: RulesValidation::Valid,
        updated_at: time.into(),
        manifest,
        signature: RulesSignatureMetadata {
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
    RulesCacheSnapshot,
) {
    let storage = Arc::new(MetadataStore::in_memory().unwrap());
    storage.migrate(&[MIGRATION]).unwrap();
    let (initial, _) = signed_cache("2001-01-01T00:00:00Z");
    let initial = serde_json::from_slice(&initial).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (next, key) = signed_cache("2002-01-01T00:00:00Z");
    let url = format!("http://{}/rules", listener.local_addr().unwrap());
    let rules = PerformanceRules::new(storage.clone(), settings(Some(url), Some(key))).unwrap();
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

async fn respond_rules(mut stream: tokio::net::TcpStream, snapshot: &RulesCacheSnapshot) {
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
        (Some(" \t\n"), 21_600),
        (Some("-1"), 21_600),
        (Some("18446744073709551616"), 21_600),
        (Some("0"), 900),
        (Some("900"), 900),
        (Some(" 1800 "), 1_800),
        (Some("86400"), 86_400),
        (Some("18446744073709551615"), 86_400),
    ] {
        assert_eq!(
            RulesSettings::new(None, None, value).refresh_interval,
            Duration::from_secs(seconds)
        );
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
    let accepted = tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await;
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
    let saved: RulesCacheSnapshot = serde_json::from_slice(&saved).unwrap();
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
        let rejected = PerformanceRules::new(storage.clone(), settings(remote, key)).unwrap();
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
    let reopened = PerformanceRules::new(
        storage.clone(),
        settings(Some(url.clone()), Some(key.clone())),
    )
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
    let saved: RulesCacheSnapshot =
        serde_json::from_slice(&storage.read(read_cache).unwrap().unwrap()).unwrap();
    assert_eq!(saved.manifest, next.manifest);
    assert_eq!(refreshed.status.generated_at, next.generated_at);
    assert_eq!(reopened.status().status.generated_at, next.generated_at);
    let latest = PerformanceRules::new(storage, settings(Some(url), Some(key))).unwrap();
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
    let interval = rules.settings.refresh_interval;
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
        .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
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
            rules = PerformanceRules::new(storage.clone(), RulesSettings::default()).unwrap();
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
    let saved: RulesCacheSnapshot =
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
    let saved: RulesCacheSnapshot =
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

fn reopen(bytes: &[u8], public_key: Option<String>) -> PerformanceRules {
    let storage = Arc::new(MetadataStore::in_memory().unwrap());
    storage.migrate(&[MIGRATION]).unwrap();
    storage
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO performance_rules(singleton,snapshot) VALUES(1,?1)",
                [bytes],
            )?;
            Ok::<_, StorageError>(())
        })
        .unwrap();
    let url = "https://rules.example.invalid/manifest.json".to_string();
    PerformanceRules::new(storage, settings(Some(url), public_key)).unwrap()
}

#[tokio::test]
async fn verified_cache_reopens_as_active_remote_rules() {
    let (bytes, key) = signed_cache("2026-05-30T10:00:00Z");
    let status = reopen(&bytes, Some(key)).status().status;
    assert_eq!(status.rule_source, RuleSource::Remote);
    assert_eq!(status.rule_channel, RuleChannel::Remote);
    assert_eq!(status.generated_at, "2026-05-30T10:00:00Z");
    assert_eq!(
        status.last_refresh_at.as_deref(),
        Some("2026-05-30T10:00:00Z")
    );
    assert_eq!(status.rules_cache.state, RulesCacheState::Recorded);
    assert!(status.rules_cache.recorded);
    assert!(status.warnings.is_empty());
}

#[tokio::test]
async fn unverifiable_cache_is_preserved_and_closes_managed_work() {
    let (bytes, key) = signed_cache("2026-05-30T10:00:00Z");
    let mut snapshot: RulesCacheSnapshot = serde_json::from_slice(&bytes).unwrap();
    let mut unsupported = snapshot.clone();
    unsupported.manifest.schema_version = 99;
    unsupported.schema_version = 99;
    let mut unsigned = serde_json::to_value(&snapshot).unwrap();
    unsigned.as_object_mut().unwrap().remove("signature");
    snapshot.signature.signature = String::new();
    for (case, bytes, key) in [
        ("malformed", b"{not json".to_vec(), Some(key.clone())),
        (
            "unsupported",
            serde_json::to_vec(&unsupported).unwrap(),
            Some(key.clone()),
        ),
        (
            "empty signature",
            serde_json::to_vec(&snapshot).unwrap(),
            Some(key.clone()),
        ),
        (
            "absent signature",
            serde_json::to_vec(&unsigned).unwrap(),
            Some(key.clone()),
        ),
        ("untrusted", bytes.clone(), None),
    ] {
        let rules = reopen(&bytes, key);
        let status = rules.status().status;
        assert_eq!(status.rule_source, RuleSource::BuiltIn, "{case}");
        assert_eq!(status.rule_channel, RuleChannel::Bundled, "{case}");
        assert_eq!(status.rules_cache.state, RulesCacheState::Invalid, "{case}");
        assert!(!status.rules_cache.recorded, "{case}");
        assert!(!status.warnings.is_empty(), "{case}");
        assert!(
            matches!(rules.refresh().await, Err(RulesWorkflowError::Changed)),
            "{case}"
        );
        assert_eq!(
            rules.storage.read(read_cache).unwrap(),
            Some(bytes),
            "{case}"
        );
    }
}
