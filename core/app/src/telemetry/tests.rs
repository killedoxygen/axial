use super::*;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

const ID: &str = "4d8fa83c-5815-4ea2-aac1-ddcc336c405e";
const NEXT_ID: &str = "9f36d907-26bd-494f-a3db-c21adf43b3da";
const PRIVATE: &str = "/Users/private/account@example.com?token=canary";

// The real reqwest transport talks only to this loopback HTTP fixture. A held
// response exposes the send's admission lifetime without a provider dependency.
struct CollectorFixture {
    host: String,
    requests: mpsc::UnboundedReceiver<Value>,
    replies: mpsc::UnboundedSender<String>,
    task: JoinHandle<()>,
}

impl CollectorFixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let (replies, mut replies_rx) = mpsc::unbounded_channel::<String>();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = timeout(Duration::from_secs(5), read_request(&mut stream))
                    .await
                    .unwrap();
                if requests_tx.send(request).is_err() {
                    return;
                }
                let Some(reply) = replies_rx.recv().await else {
                    return;
                };
                // A client may have cancelled or timed out while held here.
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            host,
            requests,
            replies,
            task,
        }
    }

    fn telemetry(&self) -> Arc<Telemetry> {
        Arc::new(Telemetry::new(Some(
            CollectorConfig::new("phc_local_fixture", &self.host, TelemetryEnvironment::Test)
                .unwrap(),
        )))
    }

    async fn request(&mut self) -> Value {
        timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn reply(&self, status: &str, headers: &str, body: &str) {
        self.replies.send(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())).unwrap();
    }

    async fn assert_idle(&mut self) {
        assert!(
            timeout(Duration::from_millis(30), self.requests.recv())
                .await
                .is_err()
        );
        assert!(!self.task.is_finished(), "collector fixture failed");
    }
}

impl Drop for CollectorFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() <= 128 * 1024, "unbounded telemetry request");
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    assert!(headers.starts_with("POST /batch/ HTTP/1.1\r\n"));
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("bounded JSON content length");
    assert!(length <= 128 * 1024);
    let received = bytes.len() - header_end;
    assert!(received <= length);
    bytes.resize(header_end + length, 0);
    stream
        .read_exact(&mut bytes[header_end + received..])
        .await
        .unwrap();
    serde_json::from_slice(&bytes[header_end..]).unwrap()
}

async fn enable(telemetry: &Telemetry) {
    telemetry.consent_change().await.publish(true, Some(ID));
}

fn ordinary_event() -> TelemetryEvent {
    TelemetryEvent::LaunchStarted {
        loader: Some(TelemetryLoader::Fabric),
    }
}

fn frontend_report() -> FrontendErrorReportRequest {
    FrontendErrorReportRequest {
        kind: FrontendErrorKind::Render,
        name: PRIVATE.into(),
        message: PRIVATE.into(),
    }
}

#[tokio::test]
async fn consent_and_valid_identity_are_required_before_any_egress() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    assert!(telemetry.export_configured());
    for identity in [
        None,
        Some(PRIVATE),
        Some("4d8fa83c58154ea2aac1ddcc336c405e"),
        Some("{4d8fa83c-5815-4ea2-aac1-ddcc336c405e}"),
    ] {
        telemetry.consent_change().await.publish(true, identity);
        assert!(!telemetry.emit(ordinary_event()));
        assert!(!telemetry.report_frontend_error(frontend_report()));
        telemetry.capture_panic();
        assert_eq!(telemetry.flush_once().await, 0);
    }
    telemetry.consent_change().await.publish(false, Some(ID));
    assert!(!telemetry.emit(ordinary_event()));
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.assert_idle().await;

    let keyless = Telemetry::new(None);
    enable(&keyless).await;
    assert!(!keyless.export_configured());
    assert!(!keyless.emit(ordinary_event()));
    assert!(!keyless.report_frontend_error(frontend_report()));
    keyless.capture_panic();
    assert_eq!(keyless.flush_once().await, 0);
}

#[tokio::test]
async fn collector_receives_only_closed_properties_and_capture_timestamp() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    // Accepted alternate casing is normalized before it becomes distinct_id.
    telemetry
        .consent_change()
        .await
        .publish(true, Some(&ID.to_uppercase()));
    let captured_after = chrono::Utc::now();
    assert!(telemetry.emit(TelemetryEvent::AppStarted {
        state_inspector: true
    }));
    assert!(telemetry.report_frontend_error(frontend_report()));
    let captured_before = chrono::Utc::now();
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 2);
    let body = fixture.request().await;
    assert_eq!(body["api_key"], "phc_local_fixture");
    assert_eq!(body.as_object().unwrap().len(), 2);
    let batch = body["batch"].as_array().unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0]["event"], "app_started");
    assert_eq!(
        batch[0]["properties"],
        json!({
            "distinct_id": ID, "$process_person_profile": false, "environment": "test",
            "app_version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "active_flags": if cfg!(debug_assertions) { vec!["dev.state-inspector"] } else { vec![] },
        })
    );
    assert_eq!(
        batch[1]["properties"],
        json!({
            "distinct_id": ID, "$process_person_profile": false, "environment": "test", "area": "frontend",
            "$exception_list": [{"type": "frontend_error", "value": "Frontend error occurred."}],
            "$exception_fingerprint": "frontend_error", "$exception_level": "error",
        })
    );
    for event in batch {
        assert_eq!(event.as_object().unwrap().len(), 3);
        let timestamp =
            chrono::DateTime::parse_from_rfc3339(event["timestamp"].as_str().unwrap()).unwrap();
        assert!(timestamp.timestamp_millis() >= captured_after.timestamp_millis());
        assert!(timestamp.timestamp_millis() <= captured_before.timestamp_millis());
    }
    assert!(!body.to_string().contains(PRIVATE));
}

#[test]
fn all_event_variants_have_fixed_names_and_bounded_properties() {
    let base = json!({"distinct_id": ID, "$process_person_profile": false, "environment": "test"});
    for (loader, key) in [
        (TelemetryLoader::Vanilla, "vanilla"),
        (TelemetryLoader::Fabric, "fabric"),
        (TelemetryLoader::Quilt, "quilt"),
        (TelemetryLoader::Forge, "forge"),
        (TelemetryLoader::NeoForge, "neoforge"),
    ] {
        assert_eq!(TelemetryLoader::from_key(key), Some(loader));
        for (event, name) in [
            (
                TelemetryEvent::LaunchStarted {
                    loader: Some(loader),
                },
                "launch_started",
            ),
            (
                TelemetryEvent::InstanceCreated {
                    loader: Some(loader),
                },
                "instance_created",
            ),
        ] {
            let mut properties = base.clone();
            properties["loader_key"] = json!(key);
            assert_eq!(
                event.batch_item(ID, "test"),
                json!({"event": name, "properties": properties})
            );
        }
    }
    for key in [PRIVATE, "Fabric", "guardian", ""] {
        assert_eq!(TelemetryLoader::from_key(key), None);
    }
    for (event, name) in [
        (
            TelemetryEvent::LaunchStarted { loader: None },
            "launch_started",
        ),
        (
            TelemetryEvent::InstanceCreated { loader: None },
            "instance_created",
        ),
    ] {
        assert_eq!(
            event.batch_item(ID, "test"),
            json!({"event": name, "properties": base})
        );
    }
    for (outcome, label) in [
        (TelemetryLaunchOutcome::Success, "success"),
        (TelemetryLaunchOutcome::Failure, "failure"),
    ] {
        let mut properties = base.clone();
        properties["outcome"] = json!(label);
        assert_eq!(
            TelemetryEvent::LaunchCompleted { outcome }.batch_item(ID, "test"),
            json!({"event": "launch_completed", "properties": properties})
        );
    }
    for (kind, fingerprint, area, summary) in [
        (
            TelemetryErrorKind::LaunchSpawnFailed,
            "launch_spawn_failed",
            "launch",
            "Game process could not start.",
        ),
        (
            TelemetryErrorKind::LaunchStartupFailed,
            "launch_startup_failed",
            "launch",
            "Game startup failed.",
        ),
        (
            TelemetryErrorKind::InstallFailed,
            "install_failed",
            "install",
            "Installation failed.",
        ),
        (
            TelemetryErrorKind::ConfigSaveFailed,
            "config_save_failed",
            "config",
            "Settings could not be saved.",
        ),
        (
            TelemetryErrorKind::StartupFailed,
            "startup_failed",
            "startup",
            "Application startup failed.",
        ),
        (
            TelemetryErrorKind::Panic,
            "panic",
            "panic",
            "Process panicked.",
        ),
        (
            TelemetryErrorKind::FrontendError,
            "frontend_error",
            "frontend",
            "Frontend error occurred.",
        ),
    ] {
        let mut properties = base.clone();
        properties["area"] = json!(area);
        properties["$exception_list"] = json!([{"type": fingerprint, "value": summary}]);
        properties["$exception_fingerprint"] = json!(fingerprint);
        properties["$exception_level"] = json!(if kind == TelemetryErrorKind::Panic {
            "fatal"
        } else {
            "error"
        });
        assert_eq!(
            TelemetryEvent::ErrorCaptured { kind }.batch_item(ID, "test"),
            json!({"event": "$exception", "properties": properties})
        );
    }
}

#[tokio::test]
async fn queue_evicts_oldest_and_sends_bounded_fifo_batches() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(TelemetryEvent::AppStarted {
        state_inspector: false
    }));
    for _ in 0..QUEUE_CAPACITY - 1 {
        assert!(telemetry.emit(ordinary_event()));
    }
    assert!(telemetry.emit(TelemetryEvent::LaunchCompleted {
        outcome: TelemetryLaunchOutcome::Success
    }));
    let mut items = Vec::new();
    for expected in [20, 20, 20, 4] {
        fixture.reply("204 No Content", "", "");
        assert_eq!(telemetry.flush_once().await, expected);
        let request = fixture.request().await;
        let batch = request["batch"].as_array().unwrap();
        assert_eq!(batch.len(), expected);
        items.extend(batch.iter().cloned());
    }
    assert_eq!(items.len(), QUEUE_CAPACITY);
    assert!(
        items[..63]
            .iter()
            .all(|event| event["event"] == "launch_started")
    );
    assert_eq!(items[63]["event"], "launch_completed");
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.assert_idle().await;
}

#[tokio::test]
async fn revocation_waits_for_admitted_send_and_fences_later_exports() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    let send = tokio::spawn({
        let telemetry = telemetry.clone();
        async move { telemetry.flush_once().await }
    });
    assert_eq!(
        fixture.request().await["batch"][0]["properties"]["distinct_id"],
        ID
    );
    let mut revoke = Box::pin(async {
        let change = telemetry.consent_change_owned().await;
        change.publish(false, None);
    });
    assert!(
        timeout(Duration::from_millis(30), &mut revoke)
            .await
            .is_err()
    );
    assert!(telemetry.emit(ordinary_event()));
    let mut later_export = Box::pin(telemetry.flush_once());
    assert!(
        timeout(Duration::from_millis(30), &mut later_export)
            .await
            .is_err()
    );
    fixture.reply("200 OK", "", "{}");
    assert_eq!(send.await.unwrap(), 1);
    timeout(Duration::from_secs(1), revoke).await.unwrap();
    assert_eq!(later_export.await, 0);
    assert!(!telemetry.emit(ordinary_event()));
    telemetry.capture_panic();
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.assert_idle().await;
}

#[tokio::test]
async fn identity_change_discards_old_queue_while_failed_change_keeps_committed_consent() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(TelemetryEvent::AppStarted {
        state_inspector: false
    }));
    // Acquiring and dropping without publication models a failed settings commit.
    drop(telemetry.consent_change_owned().await);
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 1);
    assert_eq!(
        fixture.request().await["batch"][0]["properties"]["distinct_id"],
        ID
    );

    assert!(telemetry.emit(TelemetryEvent::AppStarted {
        state_inspector: false
    }));
    telemetry
        .consent_change()
        .await
        .publish(true, Some(NEXT_ID));
    assert!(telemetry.emit(ordinary_event()));
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 1);
    let request = fixture.request().await;
    assert_eq!(request["batch"][0]["properties"]["distinct_id"], NEXT_ID);
    assert_eq!(request["batch"][0]["event"], "launch_started");
}

#[tokio::test]
async fn cancelling_settings_waiter_does_not_release_owned_consent_fence() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    let guard = telemetry.consent_change_owned().await;
    let (commit, committed) = oneshot::channel();
    let (finished, completion) = oneshot::channel();
    let accepted = tokio::spawn(async move {
        committed.await.unwrap();
        guard.publish(false, None);
        drop(guard);
        finished.send(()).unwrap();
    });
    let waiter = tokio::spawn(async move { accepted.await });
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let mut send = Box::pin(telemetry.flush_once());
    assert!(timeout(Duration::from_millis(30), &mut send).await.is_err());
    commit.send(()).unwrap();
    completion.await.unwrap();
    assert_eq!(send.await, 0);
    fixture.assert_idle().await;
}

#[tokio::test]
async fn cancelled_send_releases_admission_without_requeueing_uncertain_delivery() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    let send = tokio::spawn({
        let telemetry = telemetry.clone();
        async move { telemetry.flush_once().await }
    });
    fixture.request().await;
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());
    let guard = timeout(Duration::from_secs(1), telemetry.consent_change_owned())
        .await
        .unwrap();
    guard.publish(false, None);
    drop(guard);
    fixture.reply("200 OK", "", "{}");
    enable(&telemetry).await;
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.assert_idle().await;
}

#[tokio::test]
async fn provider_errors_drop_failed_batch_without_recursive_reporting_or_retry() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    fixture.reply("503 Service Unavailable", "", PRIVATE);
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.request().await;
    assert_eq!(telemetry.flush_once().await, 0);
    assert!(telemetry.state.lock().unwrap().errors_by_kind.is_empty());
    fixture.assert_idle().await;
    assert!(telemetry.emit(ordinary_event()));
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 1);
    assert!(!fixture.request().await.to_string().contains(PRIVATE));
}

#[tokio::test]
async fn redirects_cannot_forward_telemetry_to_another_collector() {
    let mut fixture = CollectorFixture::start().await;
    let mut destination = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    fixture.reply(
        "307 Temporary Redirect",
        &format!("Location: {}/batch/\r\n", destination.host),
        "",
    );
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.request().await;
    destination.assert_idle().await;
    assert_eq!(telemetry.flush_once().await, 0);
}

#[tokio::test]
async fn stalled_provider_is_time_bounded_and_releases_consent_fence() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    let send = tokio::spawn({
        let telemetry = telemetry.clone();
        async move { telemetry.flush_once().await }
    });
    fixture.request().await;
    assert_eq!(
        timeout(Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let guard = timeout(Duration::from_secs(1), telemetry.consent_change())
        .await
        .unwrap();
    guard.publish(false, None);
    drop(guard);
    assert_eq!(telemetry.flush_once().await, 0);
}

#[tokio::test]
async fn shutdown_joins_inflight_send_and_attempts_every_remaining_batch() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    for _ in 0..45 {
        assert!(telemetry.emit(ordinary_event()));
    }
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(telemetry.clone().run(receiver));
    assert_eq!(
        fixture.request().await["batch"].as_array().unwrap().len(),
        20
    );
    shutdown.send(true).unwrap();
    assert!(!task.is_finished());
    fixture.reply("503 Service Unavailable", "", PRIVATE);
    for expected in [20, 5] {
        assert_eq!(
            fixture.request().await["batch"].as_array().unwrap().len(),
            expected
        );
        fixture.reply("200 OK", "", "{}");
    }
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(telemetry.flush_once().await, 0);
    fixture.assert_idle().await;
}

#[tokio::test]
async fn closed_shutdown_channel_finishes_queued_work() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    let (shutdown, receiver) = watch::channel(false);
    drop(shutdown);
    fixture.reply("200 OK", "", "{}");
    timeout(Duration::from_secs(1), telemetry.run(receiver))
        .await
        .unwrap();
    assert_eq!(
        fixture.request().await["batch"].as_array().unwrap().len(),
        1
    );
    fixture.assert_idle().await;
}

#[tokio::test]
async fn error_budgets_are_per_process_and_do_not_block_ordinary_events() {
    let fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    for kind in [
        TelemetryErrorKind::LaunchSpawnFailed,
        TelemetryErrorKind::LaunchStartupFailed,
        TelemetryErrorKind::InstallFailed,
        TelemetryErrorKind::ConfigSaveFailed,
        TelemetryErrorKind::StartupFailed,
        TelemetryErrorKind::FrontendError,
    ] {
        for _ in 0..5 {
            assert!(telemetry.emit(TelemetryEvent::ErrorCaptured { kind }));
        }
        assert!(!telemetry.emit(TelemetryEvent::ErrorCaptured { kind }));
    }
    assert!(!telemetry.emit(TelemetryEvent::ErrorCaptured {
        kind: TelemetryErrorKind::Panic
    }));
    assert_eq!(telemetry.state.lock().unwrap().errors, 30);
    telemetry.consent_change().await.publish(false, None);
    enable(&telemetry).await;
    assert!(!telemetry.emit(TelemetryEvent::ErrorCaptured {
        kind: TelemetryErrorKind::FrontendError
    }));
    assert!(telemetry.emit(ordinary_event()));
    let restarted = fixture.telemetry();
    enable(&restarted).await;
    assert!(restarted.emit(TelemetryEvent::ErrorCaptured {
        kind: TelemetryErrorKind::FrontendError
    }));
}

#[tokio::test]
async fn panic_capture_never_waits_for_state_and_retains_only_fixed_error() {
    let mut fixture = CollectorFixture::start().await;
    let telemetry = fixture.telemetry();
    enable(&telemetry).await;
    let held = telemetry.state.lock().unwrap();
    telemetry.capture_panic();
    assert!(held.queue.is_empty());
    drop(held);
    telemetry.capture_panic();
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 1);
    let request = fixture.request().await;
    assert_eq!(
        request["batch"][0]["properties"]["$exception_list"],
        json!([{"type": "panic", "value": "Process panicked."}])
    );
    assert_eq!(
        request["batch"][0]["properties"]["$exception_level"],
        "fatal"
    );
}

#[test]
fn collector_configuration_rejects_unsafe_destinations_and_keeps_errors_fixed() {
    for host in [
        "http://example.com",
        "http://localhost.example.com",
        "https://user:password@example.com",
        "https://example.com?token=private",
        "https://example.com#private",
        "file:///private",
        "not a URL",
    ] {
        let error = CollectorConfig::new("phc_local_fixture", host, TelemetryEnvironment::Test)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "invalid telemetry collector configuration"
        );
    }
    for key in [
        "",
        "phc_a",
        "private-key",
        "phc_key with space",
        "phc_key\ncanary",
        &format!("phc_{}", "x".repeat(125)),
    ] {
        assert!(
            CollectorConfig::new(key, "https://example.com", TelemetryEnvironment::Test).is_err()
        );
    }
    for host in [
        "https://example.com/base",
        "http://localhost",
        "http://127.0.0.1:1",
        "http://[::1]:1",
    ] {
        assert!(
            CollectorConfig::new("phc_local_fixture", host, TelemetryEnvironment::Test).is_ok()
        );
    }
}

#[test]
fn frontend_requests_are_closed_and_bounded_before_admission() {
    for invalid in [
        json!({"kind": "private", "name": "Error", "message": "safe"}),
        json!({"kind": "render", "name": "Error", "message": "safe", "stack": PRIVATE}),
        json!({"kind": "render", "message": "safe"}),
    ] {
        assert!(serde_json::from_value::<FrontendErrorReportRequest>(invalid).is_err());
    }
    for kind in ["error", "unhandledrejection", "render"] {
        let report: FrontendErrorReportRequest = serde_json::from_value(
            json!({"kind": kind, "name": "n".repeat(64), "message": "é".repeat(200)}),
        )
        .unwrap();
        assert!(report.is_bounded());
    }
    let mut report = frontend_report();
    report.name = "n".repeat(65);
    assert!(!report.is_bounded());
    report.name = "Error".into();
    report.message = "é".repeat(201);
    assert!(!report.is_bounded());
}

#[tokio::test]
async fn explicit_custom_environment_is_normalized_and_validated_before_export() {
    let mut fixture = CollectorFixture::start().await;
    let environment = TelemetryEnvironment::from_label("  Release_EU-2  ").unwrap();
    let telemetry = Telemetry::new(Some(
        CollectorConfig::new("phc_local_fixture", &fixture.host, environment).unwrap(),
    ));
    enable(&telemetry).await;
    assert!(telemetry.emit(ordinary_event()));
    fixture.reply("200 OK", "", "{}");
    assert_eq!(telemetry.flush_once().await, 1);
    assert_eq!(
        fixture.request().await["batch"][0]["properties"]["environment"],
        "release_eu-2"
    );
    for invalid in [
        "",
        PRIVATE,
        "user@example.com",
        "two labels",
        "café",
        &"x".repeat(33),
    ] {
        assert!(TelemetryEnvironment::from_label(invalid).is_none());
        assert!(
            CollectorConfig::new(
                "phc_local_fixture",
                &fixture.host,
                TelemetryEnvironment::Custom(invalid.into())
            )
            .is_err()
        );
    }
    assert!(TelemetryEnvironment::from_label(&"x".repeat(32)).is_some());
    fixture.assert_idle().await;
}
