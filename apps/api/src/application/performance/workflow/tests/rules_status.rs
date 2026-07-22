use super::*;

#[tokio::test]
async fn status_reports_bundled_rules_without_remote_refresh() {
    let fixture = TestFixture::new("status");

    let Json(response) = handle_status(State(fixture.state.clone()))
        .await
        .expect("status should serialize");
    let status = &response.status;

    assert_eq!(status.rule_source, axial_performance::RuleSource::BuiltIn);
    assert_eq!(status.rule_channel, axial_performance::RuleChannel::Bundled);
    assert!(!status.rules_cache.recorded);
    assert_eq!(
        status.rules_cache.state,
        axial_performance::RulesCacheState::Unavailable
    );
    assert!(status.rules_cache.updated_at.is_none());
    assert!(status.rules_cache.loaded_at.is_none());
    assert!(status.rules_cache.warning.is_none());
    assert_eq!(
        status.schema_version,
        axial_performance::PERFORMANCE_MANIFEST_SCHEMA_VERSION
    );
    assert!(!status.generated_at.is_empty());
    assert!(status.composition_count > 0);
    assert!(!status.remote_refresh);
    assert_eq!(status.last_refresh_at, None);
    assert!(response.guardian_facts.is_empty());
    assert_eq!(status.validation, axial_performance::RulesValidation::Valid);
    assert_eq!(
        status.health_states,
        vec![
            BundleHealth::Healthy,
            BundleHealth::Disabled,
            BundleHealth::Invalid,
        ]
    );
    let encoded = serde_json::to_string(&response).expect("serialize status response");
    assert!(!encoded.contains("degraded"));
    assert!(!encoded.contains("fallback"));
    assert_eq!(
        status.ownership_classes,
        vec![
            axial_performance::OwnershipClass::CompositionManaged,
            axial_performance::OwnershipClass::UserManaged,
        ]
    );
    assert!(status.warnings.is_empty());
}

#[tokio::test]
async fn status_reports_invalid_remote_rules_with_guardian_fact_and_safe_copy() {
    let root = test_root("status-invalid-remote-rules");
    let paths = test_paths(&root);
    let mut remote = axial_performance::builtin_manifest().expect("builtin manifest");
    remote.schema_version = 99;
    let signed = signed_rules_response(&remote);
    let cache_path = paths.performance_dir().join("rules-cache.json");
    fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("create cache dir");
    fs::write(
        &cache_path,
        serde_json::to_vec(&axial_performance::RulesCacheSnapshot {
            rule_source: axial_performance::RuleSource::Remote,
            rule_channel: axial_performance::RuleChannel::Remote,
            schema_version: remote.schema_version,
            generated_at: remote.generated_at.clone(),
            validation: axial_performance::RulesValidation::Valid,
            updated_at: "2026-06-15T12:00:00Z".to_string(),
            manifest: remote,
            signature: axial_performance::RulesSignatureMetadata {
                signature: signed.signature,
                key_id: Some("test-key".to_string()),
            },
        })
        .expect("serialize invalid remote cache"),
    )
    .expect("write invalid remote cache");
    let remote_url =
        "https://rules.example.test/private-feed/performance.json?api_token=secret-token";
    let state = build_test_state(&root, Some(remote_url.to_string()), Some(signed.public_key));

    let response = router()
        .with_state(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/performance/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = String::from_utf8(body.to_vec()).expect("utf8 body");
    let value: serde_json::Value = serde_json::from_str(&body).expect("status json");

    assert_eq!(value["rule_source"], "built_in");
    assert_eq!(value["rule_channel"], "bundled");
    assert_eq!(value["rules_cache"]["state"], "invalid");
    assert!(
        value["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("Remote rules cache was invalid")))
    );
    let fact = value["guardian_facts"]
        .as_array()
        .expect("guardian facts")
        .iter()
        .find(|fact| fact["id"] == "performance_rules_invalid")
        .expect("invalid rules fact");
    assert_eq!(fact["domain"], "Performance");
    assert_eq!(fact["severity"], "Degraded");
    assert_eq!(fact["confidence"], "High");
    assert_eq!(fact["ownership"], "ExternalProviderDerived");
    assert_omits_raw_fragments(
        &body,
        &[
            remote_url,
            "private-feed",
            "api_token",
            "secret-token",
            &cache_path.display().to_string(),
        ],
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn rules_refresh_route_requires_configured_remote_url() {
    let fixture = TestFixture::new("rules-refresh-unconfigured");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("error json");
    assert_eq!(
        value,
        serde_json::json!({ "error": "performance remote rules url is not configured" })
    );
    let journal = fixture
        .state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
        .expect("refresh journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );
    assert_eq!(
        journal.failure_point.as_deref(),
        Some("refresh_remote_rules")
    );
    assert_eq!(
        journal.outcome,
        Some(crate::state::contracts::OperationOutcome::Failed)
    );
    assert!(journal.targets.iter().any(|target| {
        target.id == "performance_rules_remote_source"
            && target.ownership == crate::state::contracts::OwnershipClass::ExternalProviderDerived
    }));
    assert!(journal.targets.iter().any(|target| {
        target.id == "performance_rules_cache"
            && target.ownership == crate::state::contracts::OwnershipClass::LauncherManaged
    }));
}

#[tokio::test]
async fn rules_refresh_journal_failure_prevents_refresh() {
    let fixture = TestFixture::new("rules-refresh-journal-failure");
    let before = fixture.state.performance().rules_status();
    let journal_path = fixture
        .root
        .join("config")
        .join("state")
        .join("operation-journals.json");
    fs::create_dir_all(journal_path).expect("block journal snapshot destination");

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = String::from_utf8(body.to_vec()).expect("utf8 body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("error json"),
        serde_json::json!({
            "error": "Could not load performance data. Check app data permissions and try again."
        })
    );
    assert_omits_raw_fragments(
        &body,
        &[
            "operation-journals.json",
            "/Users/alice",
            "C:\\Users\\Alice",
        ],
    );

    let after = fixture.state.performance().rules_status();
    assert_eq!(after.rule_source, before.rule_source);
    assert_eq!(after.rule_channel, before.rule_channel);
    assert_eq!(after.generated_at, before.generated_at);
    assert_eq!(after.last_refresh_at, before.last_refresh_at);
    assert!(
        fixture
            .state
            .journals()
            .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
            .is_none()
    );
}

#[tokio::test]
async fn rules_refresh_route_accepts_configured_remote_manifest() {
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = "2026-05-30T13:00:00Z".to_string();
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
    )
    .await;
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "rules-refresh-configured",
        Some(remote_url),
        Some(signed.public_key),
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let status: axial_performance::PerformanceRulesStatus =
        serde_json::from_slice(&body).expect("rules status json");
    assert_eq!(status.rule_source, axial_performance::RuleSource::Remote);
    assert_eq!(status.rule_channel, axial_performance::RuleChannel::Remote);
    assert!(status.remote_refresh);
    assert_eq!(status.generated_at, manifest.generated_at);
    assert_eq!(status.validation, axial_performance::RulesValidation::Valid);
    assert!(status.warnings.is_empty());
    let journal = fixture
        .state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
        .expect("refresh journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    assert_eq!(journal.failure_point, None);
    assert_eq!(
        journal.outcome,
        Some(crate::state::contracts::OperationOutcome::Succeeded)
    );
    assert_eq!(journal.planned_steps.len(), 1);
    assert_eq!(journal.completed_steps.len(), 1);
    assert!(journal.targets.iter().any(|target| {
        target.id == "performance_rules_remote_source"
            && target.ownership == crate::state::contracts::OwnershipClass::ExternalProviderDerived
    }));
    assert!(journal.targets.iter().any(|target| {
        target.id == "performance_rules_cache"
            && target.ownership == crate::state::contracts::OwnershipClass::LauncherManaged
    }));
    assert_eq!(
        journal.completed_steps[0]
            .changed_target
            .as_ref()
            .map(|target| target.ownership),
        Some(crate::state::contracts::OwnershipClass::LauncherManaged)
    );
}

#[tokio::test]
async fn slow_rules_provider_runs_after_planned_ownership_without_false_timeout() {
    let root = test_root("rules-refresh-slow-provider");
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = "2026-05-30T13:05:00Z".to_string();
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_delayed_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
        Duration::from_millis(700),
    )
    .await;
    let state = build_test_state(&root, Some(remote_url), Some(signed.public_key));

    let response = router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        state.performance().rules_status().generated_at,
        manifest.generated_at
    );
    let journal = state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
        .expect("slow refresh journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Succeeded
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn terminal_journal_failure_returns_bounded_then_reconciles_without_refresh_replay() {
    let root = test_root("rules-refresh-terminal-journal-retry");
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = "2026-05-30T13:06:00Z".to_string();
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_delayed_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        Some(signed.signature),
        Duration::from_millis(100),
    )
    .await;
    let journal_backend = Arc::new(ScriptedOperationBackend::default());
    let status_backend = Arc::new(ScriptedOperationBackend::default());
    journal_backend.gate_attempt(1);
    let base = build_test_state(&root, Some(remote_url), Some(signed.public_key));
    let state = replace_operation_backends(base, &root, journal_backend.clone(), status_backend);
    let request_state = state.clone();
    let request = tokio::spawn(async move {
        router()
            .with_state(request_state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/performance/rules/refresh")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response")
    });
    journal_backend.wait_for_attempt(1).await;
    journal_backend.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .journals()
                .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("planned rules journal");
    journal_backend.set_fail_all(true);

    let response = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .expect("bounded terminal persistence response")
        .expect("request task");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        state.performance().rules_status().generated_at,
        manifest.generated_at
    );

    journal_backend.set_fail_all(false);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .journals()
                .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
                .is_some_and(|journal| {
                    journal.status == crate::state::contracts::OperationStatus::Succeeded
                })
                && !state.journals().has_retry_candidate()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal journal reconciliation");

    assert_eq!(
        state.performance().rules_status().generated_at,
        manifest.generated_at
    );
    assert!(!state.journals().has_retry_candidate());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn rules_refresh_route_provider_failure_keeps_previous_rules_and_redacts_provider_details() {
    let builtin = axial_performance::builtin_manifest().expect("builtin manifest");
    let signed = signed_rules_response(&builtin);
    let remote_base_url = spawn_closing_rules_server().await;
    let remote_url =
        format!("{remote_base_url}/private-feed/performance.json?api_token=secret-token");
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "rules-refresh-provider-failure",
        Some(remote_url.clone()),
        Some(signed.public_key),
    );
    let before = fixture.state.performance().rules_status();

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = String::from_utf8(body.to_vec()).expect("utf8 body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("error json"),
        serde_json::json!({
            "error": "Performance rules provider response could not be verified. Try again later."
        })
    );
    let status = fixture.state.performance().rules_status();

    assert_eq!(status.rule_source, before.rule_source);
    assert_eq!(status.rule_channel, before.rule_channel);
    assert_eq!(status.generated_at, before.generated_at);
    assert_eq!(status.validation, axial_performance::RulesValidation::Valid);
    assert!(
        status
            .warnings
            .iter()
            .any(|warning| warning.contains("Remote rules refresh rejected: request failed"))
    );
    assert_omits_raw_fragments(
        &body,
        &[
            &remote_url,
            "127.0.0.1",
            "private-feed",
            "performance.json",
            "api_token",
            "secret-token",
        ],
    );
    let journal = fixture
        .state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
        .expect("refresh journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );
    assert_eq!(
        journal.failure_point.as_deref(),
        Some("refresh_remote_rules")
    );
    assert_eq!(
        journal.outcome,
        Some(crate::state::contracts::OperationOutcome::Failed)
    );
    assert_eq!(journal.completed_steps.len(), 1);
    assert_eq!(
        journal.completed_steps[0].result,
        crate::state::contracts::OperationStepResult::Failed
    );
    assert_eq!(journal.completed_steps[0].changed_target, None);
    assert!(journal.targets.iter().any(|target| {
        target.id == "performance_rules_remote_source"
            && target.ownership == crate::state::contracts::OwnershipClass::ExternalProviderDerived
    }));
}

#[tokio::test]
async fn rules_refresh_route_provider_rate_limit_and_invalid_body_are_bounded_and_redacted() {
    let builtin = axial_performance::builtin_manifest().expect("builtin manifest");
    let signed = signed_rules_response(&builtin);
    let cases = [
        (
            "rules-refresh-provider-rate-limit",
            "429 Too Many Requests",
            b"{\"provider_payload\":{\"token\":\"secret-rate-limit\"}}".to_vec(),
            None,
            vec!["provider_payload", "secret-rate-limit"],
        ),
        (
            "rules-refresh-provider-invalid-body",
            "200 OK",
            b"{\"provider_payload\":\"secret-invalid-body\"".to_vec(),
            Some(signed.signature.clone()),
            vec!["provider_payload", "secret-invalid-body"],
        ),
    ];

    for (name, status_line, body, signature, sensitive_fragments) in cases {
        let remote_base_url = spawn_rules_provider_response(status_line, body, signature).await;
        let remote_url =
            format!("{remote_base_url}/private-feed/performance.json?api_token=secret-token");
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            name,
            Some(remote_url.clone()),
            Some(signed.public_key.clone()),
        );
        let before = fixture.state.performance().rules_status();

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/performance/rules/refresh")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let body = String::from_utf8(body.to_vec()).expect("utf8 body");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).expect("error json"),
            serde_json::json!({
                "error": "Performance rules provider response could not be verified. Try again later."
            })
        );
        let status = fixture.state.performance().rules_status();

        assert_eq!(status.rule_source, before.rule_source);
        assert_eq!(status.rule_channel, before.rule_channel);
        assert_eq!(status.generated_at, before.generated_at);
        assert_eq!(status.validation, axial_performance::RulesValidation::Valid);
        assert!(
            !status.warnings.is_empty(),
            "{name} should retain a bounded warning"
        );
        assert!(
            status.rule_source == axial_performance::RuleSource::BuiltIn,
            "{name} should keep valid fallback rules"
        );
        let mut omitted = vec![
            remote_url.as_str(),
            "127.0.0.1",
            "private-feed",
            "performance.json",
            "api_token",
            "secret-token",
        ];
        omitted.extend(sensitive_fragments.iter().copied());
        assert_omits_raw_fragments(&body, &omitted);
        assert_refresh_journal_failed_without_cache_change(&fixture.state);
    }
}

#[tokio::test]
async fn rules_refresh_route_rejects_missing_signature_and_keeps_builtin_rules() {
    let mut manifest = axial_performance::builtin_manifest().expect("builtin manifest");
    manifest.generated_at = "2026-05-30T13:30:00Z".to_string();
    let signed = signed_rules_response(&manifest);
    let remote_url = spawn_rules_server(
        serde_json::to_vec(&manifest).expect("serialize remote manifest"),
        None,
    )
    .await;
    let fixture = TestFixture::new_with_remote_url_and_public_key(
        "rules-refresh-missing-signature",
        Some(remote_url),
        Some(signed.public_key),
    );

    let response = router()
        .with_state(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/performance/rules/refresh")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("route response");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("error json"),
        serde_json::json!({
            "error": "Performance rules provider response could not be verified. Try again later."
        })
    );
    let status = fixture.state.performance().rules_status();
    assert_eq!(status.rule_source, axial_performance::RuleSource::BuiltIn);
    assert!(status.remote_refresh);
    assert!(
        status
            .warnings
            .iter()
            .any(|warning| warning.contains("signature header is missing"))
    );
    assert_refresh_journal_failed_without_cache_change(&fixture.state);
}

async fn spawn_closing_rules_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind closing rules server");
    let addr = listener.local_addr().expect("closing rules server addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept rules request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
    });
    format!("http://{addr}")
}

async fn spawn_rules_provider_response(
    status_line: &str,
    body: Vec<u8>,
    signature: Option<String>,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rules provider response server");
    let addr = listener.local_addr().expect("rules provider server addr");
    let status_line = status_line.to_string();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept rules request");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        let signature_header = signature
            .as_ref()
            .map(|signature| {
                format!(
                    "{}: {}\r\n{}: test-key\r\n",
                    axial_performance::RULES_SIGNATURE_HEADER,
                    signature,
                    axial_performance::RULES_KEY_ID_HEADER
                )
            })
            .unwrap_or_default();
        let header = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
            signature_header,
            body.len()
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write rules provider response header");
        socket
            .write_all(&body)
            .await
            .expect("write rules provider response body");
    });
    format!("http://{addr}")
}

fn assert_refresh_journal_failed_without_cache_change(state: &AppState) {
    let journal = state
        .journals()
        .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
        .expect("refresh journal");
    assert_eq!(
        journal.status,
        crate::state::contracts::OperationStatus::Failed
    );
    assert_eq!(
        journal.failure_point.as_deref(),
        Some("refresh_remote_rules")
    );
    assert_eq!(
        journal.outcome,
        Some(crate::state::contracts::OperationOutcome::Failed)
    );
    assert_eq!(journal.completed_steps.len(), 1);
    assert_eq!(
        journal.completed_steps[0].result,
        crate::state::contracts::OperationStepResult::Failed
    );
    assert_eq!(journal.completed_steps[0].changed_target, None);
}

#[test]
fn bounded_performance_data_error_omits_raw_internal_details() {
    let raw_parser = serde_json::from_str::<serde_json::Value>("{not json")
        .expect_err("invalid json")
        .to_string();
    let raw_error = format!(
        "failed to read /home/zero/.config/axial/performance.json and C:\\Users\\Zero\\AppData\\Roaming\\Axial\\performance.json: {raw_parser}: Permission denied (os error 13)"
    );

    let error = internal_error(&raw_error);
    let body = json_error_message(&error);

    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, PERFORMANCE_DATA_INTERNAL_ERROR);
    assert_omits_raw_fragments(
        &body,
        &[
            "/home/zero/.config/axial/performance.json",
            "C:\\Users\\Zero\\AppData\\Roaming\\Axial\\performance.json",
            raw_parser.as_str(),
            "Permission denied",
            "os error 13",
        ],
    );
}

#[test]
fn bounded_install_io_error_omits_raw_os_details() {
    let cases = [performance_install_error(InstallError::Io(
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        ),
    ))];

    for error in cases {
        let body = json_error_message(&error);

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, PERFORMANCE_INSTALL_INTERNAL_ERROR);
        assert_omits_raw_fragments(&body, &["Permission denied", "os error 13"]);
    }
}
