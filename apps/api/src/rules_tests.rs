use super::tests::{read_rules_http_json, run_rules_http_child};
use super::*;
#[tokio::test]
async fn explicit_rules_refresh_survives_http_waiter_loss() {
    use ed25519_dalek::{Signer, SigningKey};
    use futures_util::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(
        listener: &TcpListener,
        budget: &mut usize,
    ) -> Result<(tokio::net::TcpStream, &'static str), &'static str> {
        *budget = budget
            .checked_sub(8192)
            .ok_or("provider request budget exhausted")?;
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|_| "provider accept failed")?;
        if !peer.ip().is_loopback() {
            return Err("provider peer was not loopback");
        }
        let mut bytes = vec![0; 8192];
        let mut length = 0;
        while !bytes[..length].windows(4).any(|part| part == b"\r\n\r\n") {
            if length == bytes.len() {
                return Err("provider request exceeded its bound");
            }
            let count = stream
                .read(&mut bytes[length..])
                .await
                .map_err(|_| "provider read failed")?;
            if count == 0 {
                return Err("provider request ended early");
            }
            length += count;
        }
        let path = ["/rules", "/observed", "/release"]
            .into_iter()
            .find(|path| bytes[..length].starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()))
            .ok_or("provider request did not match a fixed route")?;
        if !bytes[..length].ends_with(b"\r\n\r\n") {
            return Err("provider request did not match its fixed phase");
        }
        // These fixture-control requests never carry the application's capability.
        let headers =
            std::str::from_utf8(&bytes[..length]).map_err(|_| "provider headers were invalid")?;
        for header in headers.lines().skip(1) {
            if let Some((name, _)) = header.split_once(':')
                && (name.eq_ignore_ascii_case(transport::CAPABILITY_HEADER)
                    || name.eq_ignore_ascii_case("authorization"))
            {
                return Err("provider received an unexpected authentication header");
            }
        }
        Ok((stream, path))
    }

    async fn respond(
        mut stream: tokio::net::TcpStream,
        headers: &str,
        body: &[u8],
        budget: &mut usize,
    ) -> Result<(), &'static str> {
        *budget = budget
            .checked_sub(headers.len() + body.len())
            .ok_or("provider response budget exhausted")?;
        stream
            .write_all(headers.as_bytes())
            .await
            .map_err(|_| "provider headers failed")?;
        stream
            .write_all(body)
            .await
            .map_err(|_| "provider body failed")?;
        stream.shutdown().await.map_err(|_| "provider close failed")
    }

    let key = SigningKey::from_bytes(&[29; 32]);
    let public_key = hex::encode(key.verifying_key().to_bytes());
    let mut manifest = axial_performance::builtin_manifest().unwrap();
    let mut signed = Vec::with_capacity(2);
    for generated_at in ["2001-01-01T00:00:00Z", "2001-02-01T00:00:00Z"] {
        manifest.generated_at = generated_at.into();
        let body = axial_performance::canonical_manifest_payload(&manifest).unwrap();
        assert!(body.len() <= 1024 * 1024);
        let signature = hex::encode(key.sign(&body).to_bytes());
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-axial-rules-signature-ed25519: {signature}\r\nConnection: close\r\n\r\n",
            body.len(),
        );
        signed.push((headers, body));
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rules", listener.local_addr().unwrap());
    let (child_finished, mut finished) = tokio::sync::oneshot::channel::<()>();
    let provider = tokio::spawn(async move {
        let serve = async {
            let mut reads = 5 * 8192 + 1;
            let mut writes = 2 * 1024 * 1024 + 8192;
            let (startup, path) = request(&listener, &mut reads).await?;
            if path != "/rules" {
                return Err("startup provider request was not rules");
            }
            respond(startup, &signed[0].0, &signed[0].1, &mut writes).await?;
            // The control connection may arrive before API-to-provider I/O.
            let (left, left_path) = request(&listener, &mut reads).await?;
            let (right, right_path) = request(&listener, &mut reads).await?;
            let (mut explicit, observed) = match (left_path, right_path) {
                ("/rules", "/observed") => (left, right),
                ("/observed", "/rules") => (right, left),
                _ => return Err("explicit refresh and observation were not paired"),
            };
            respond(
                observed,
                "HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\n",
                br#"{"observed":true}"#,
                &mut writes,
            )
            .await?;
            let (release, path) = request(&listener, &mut reads).await?;
            if path != "/release" {
                return Err("provider release arrived out of order");
            }

            reads = reads
                .checked_sub(1)
                .ok_or("upstream probe budget exhausted")?;
            let mut byte = [0];
            // Observation only: an open upstream is valid if Hyper retained the handler.
            let read =
                match tokio::time::timeout(Duration::from_millis(250), explicit.read(&mut byte))
                    .await
                {
                    Err(_) => "open".to_string(),
                    Ok(Ok(0)) => "eof".to_string(),
                    Ok(Ok(_)) => return Err("provider received unexpected trailing request bytes"),
                    Ok(Err(error)) => format!("io:{:?}", error.kind()),
                };
            writes = writes
                .checked_sub(signed[1].0.len() + signed[1].1.len())
                .ok_or("provider response budget exhausted")?;
            let headers = explicit.write_all(signed[1].0.as_bytes()).await;
            let body = explicit.write_all(&signed[1].1).await;
            let close = explicit.shutdown().await;
            drop(explicit);
            let outcome = |result: io::Result<()>| match result {
                Ok(()) => "ok".to_string(),
                Err(error) => format!("io:{:?}", error.kind()),
            };
            let proof = serde_json::to_vec(&serde_json::json!({
                "upstream_read": read,
                "headers": outcome(headers),
                "body": outcome(body),
                "close": outcome(close),
            }))
            .map_err(|_| "provider proof could not encode")?;
            if proof.len() > 512 {
                return Err("provider proof exceeded its bound");
            }
            eprintln!(
                "[rules-waiter] provider={}",
                std::str::from_utf8(&proof).map_err(|_| "provider proof was invalid UTF-8")?,
            );
            respond(
                release,
                &format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    proof.len()
                ),
                &proof,
                &mut writes,
            )
            .await?;
            let (reopened, path) = request(&listener, &mut reads).await?;
            if path != "/rules" {
                return Err("cold provider request was not rules");
            }
            respond(
                reopened,
                "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                &[],
                &mut writes,
            )
            .await?;
            Ok::<_, &'static str>(())
        };
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(120), serve) => result.map_err(|_| "provider exceeded its deadline")?,
            _ = &mut finished => Err("child ended before the fixed provider journey completed"),
        }
    });
    let child = std::panic::AssertUnwindSafe(run_rules_http_child(
        "rules_tests::rules_waiter_loss_helper",
        &url,
        &public_key,
    ))
    .catch_unwind()
    .await;
    let _ = child_finished.send(());
    let provided = provider.await;
    let temporary = child.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    if !matches!(provided, Ok(Ok(()))) {
        panic!(
            "rules waiter provider did not join its fixed journey: {provided:?}; retained {}",
            temporary.keep().display()
        );
    }
}

#[tokio::test]
#[ignore = "rules waiter child isolates process-wide trust configuration"]
async fn rules_waiter_loss_helper() {
    use axial_app::performance::model::PerformanceRulesStatusResponse;
    use axial_performance::{RuleChannel, RuleSource, RulesCacheState, RulesValidation};
    use futures_util::FutureExt;
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};
    use tokio::io::AsyncWriteExt;

    async fn control(
        client: &reqwest::Client,
        base: &str,
        phase: &str,
        budget: &mut usize,
    ) -> Result<Value, &'static str> {
        *budget = budget
            .checked_sub(512)
            .ok_or("fixture control budget exhausted")?;
        let mut response = client
            .get(format!("{base}/{phase}"))
            .send()
            .await
            .map_err(|_| "fixture control failed")?;
        if response.status() != StatusCode::OK
            || response.content_length().is_none_or(|size| size > 512)
        {
            return Err("fixture control response was invalid");
        }
        let mut bytes = Vec::with_capacity(512);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "fixture control body failed")?
        {
            if chunk.len() > 512 - bytes.len() {
                return Err("fixture control response exceeded its bound");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "fixture control was not JSON")
    }

    async fn observe(
        client: &reqwest::Client,
        bootstrap: &ApiTransportBootstrap,
        budget: &mut usize,
        generated_at: Option<&str>,
    ) -> Result<(StatusCode, Value), &'static str> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut last = None;
            for _ in 0..64 {
                let response = read_rules_http_json(
                    client,
                    bootstrap,
                    Method::GET,
                    "/api/v1/performance/status",
                    budget,
                )
                .await?;
                if response.0 != StatusCode::OK {
                    return Err("rules status was unavailable");
                }
                let decoded: PerformanceRulesStatusResponse =
                    serde_json::from_value(response.1.clone())
                        .map_err(|_| "rules status did not decode")?;
                let ready = decoded.status.rule_source == RuleSource::Remote
                    && match generated_at {
                        Some(expected) => decoded.status.generated_at == expected,
                        None => decoded.status.rules_cache.warning.is_some(),
                    };
                last = Some(response);
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            last.ok_or("rules observation produced no response")
        })
        .await
        .map_err(|_| "rules observation exceeded its deadline")?
    }

    let root = PathBuf::from(std::env::var_os("AXIAL_TEST_RULES_PROFILE").unwrap());
    let remote = std::env::var(axial_performance::PERFORMANCE_RULES_URL_ENV).unwrap();
    let base = remote.strip_suffix("/rules").unwrap();
    let provider_address: std::net::SocketAddr =
        base.strip_prefix("http://").unwrap().parse().unwrap();
    assert!(provider_address.ip().is_loopback());
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut response_budget = (3 * 64 + 3) * 64 * 1024;
    let mut control_budget = 2 * 512;
    let mut observations = Vec::with_capacity(6);
    let mut upstream = None;
    let mut caller_closed = false;
    for first in [true, false] {
        let services = match start_in_profile(root.clone(), Some("http://localhost:1420")).await {
            Ok(services) => services,
            Err(failure) => {
                if let Err(retained) = failure.try_preserve() {
                    std::mem::forget(retained);
                }
                panic!("rules waiter profile could not start; parent retains fixture");
            }
        };
        let bootstrap = services.server.bootstrap();
        let reads = std::panic::AssertUnwindSafe(async {
            let ready = observe(&client, &bootstrap, &mut response_budget, if first { Some("2001-01-01T00:00:00Z") } else { None }).await?;
            if first {
                let decoded: PerformanceRulesStatusResponse = serde_json::from_value(ready.1.clone()).map_err(|_| "startup status did not decode")?;
                if decoded.status.rule_source != RuleSource::Remote
                    || decoded.status.generated_at != "2001-01-01T00:00:00Z"
                    || decoded.status.validation != RulesValidation::Valid
                    || decoded.status.rules_cache.state != RulesCacheState::Recorded
                    || !decoded.status.rules_cache.recorded
                    || decoded.status.rules_cache.warning.is_some()
                {
                    return Err("signed startup prerequisite was not established");
                }
            }
            observations.push(ready);
            observations.push(read_rules_http_json(&client, &bootstrap, Method::GET, "/api/v1/config", &mut response_budget).await?);
            if first {
                let address: std::net::SocketAddr = bootstrap.base_url.strip_prefix("http://").ok_or("API origin was invalid")?.parse().map_err(|_| "API address was invalid")?;
                if !address.ip().is_loopback() || bootstrap.capability.len() > 4096 {
                    return Err("API caller admission was invalid");
                }
                let request = format!("POST /api/v1/performance/rules/refresh HTTP/1.1\r\nHost: {address}\r\nOrigin: http://localhost:1420\r\n{}: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", transport::CAPABILITY_HEADER, bootstrap.capability);
                if request.len() > 8192 { return Err("API caller request exceeded its bound"); }
                let mut caller = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(address)).await.map_err(|_| "API caller connect timed out")?.map_err(|_| "API caller connect failed")?;
                tokio::time::timeout(Duration::from_secs(10), caller.write_all(request.as_bytes())).await.map_err(|_| "API caller send timed out")?.map_err(|_| "API caller send failed")?;
                if control(&client, base, "observed", &mut control_budget).await? != json!({"observed":true}) {
                    return Err("explicit provider request was not observed");
                }
                let caller = caller.into_std().map_err(|_| "API caller could not close")?;
                caller.shutdown(std::net::Shutdown::Both).map_err(|_| "API caller shutdown failed")?;
                drop(caller);
                caller_closed = true;
                upstream = Some(control(&client, base, "release", &mut control_budget).await?);
                observations.push(observe(&client, &bootstrap, &mut response_budget, Some("2001-02-01T00:00:00Z")).await?);
                observations.push(read_rules_http_json(&client, &bootstrap, Method::GET, "/api/v1/config", &mut response_budget).await?);
            }
            Ok::<_, &'static str>(())
        }).catch_unwind().await;
        let shutdown = std::panic::AssertUnwindSafe(tokio::time::timeout(
            Duration::from_secs(45),
            services.server.shutdown(),
        ))
        .catch_unwind()
        .await;
        let settled = services.server.is_shutdown_settled();
        if !matches!(shutdown, Ok(Ok(Ok(())))) || !settled {
            std::mem::forget(services);
            panic!("rules waiter shutdown did not settle; parent retains fixture");
        }
        drop(services);
        reads
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .unwrap();
    }

    assert!(caller_closed);
    let upstream = upstream.unwrap();
    assert_eq!(observations.len(), 6);
    for response in &observations {
        assert_eq!(response.0, StatusCode::OK);
    }
    assert_eq!(observations[3].1, observations[1].1);
    assert_eq!(observations[5].1, observations[1].1);
    let published: PerformanceRulesStatusResponse =
        serde_json::from_value(observations[2].1.clone()).unwrap();
    assert_eq!(
        published.status.generated_at, "2001-02-01T00:00:00Z",
        "caller loss discarded the explicit refresh"
    );
    assert_eq!(published.status.rule_source, RuleSource::Remote);
    assert_eq!(published.status.rule_channel, RuleChannel::Remote);
    assert_eq!(published.status.validation, RulesValidation::Valid);
    assert_eq!(
        published.status.rules_cache.state,
        RulesCacheState::Recorded
    );
    assert!(published.status.rules_cache.recorded && published.status.remote_refresh);
    assert!(published.status.rules_cache.warning.is_none() && published.status.warnings.is_empty());
    assert!(
        published.status.last_refresh_at.is_some()
            && published.status.rules_cache.loaded_at.is_some()
    );
    assert_eq!(
        published.status.last_refresh_at,
        published.status.rules_cache.updated_at
    );
    assert_eq!(upstream["headers"], "ok");
    assert_eq!(upstream["body"], "ok");
    assert_eq!(upstream["close"], "ok");
    let reopened: PerformanceRulesStatusResponse =
        serde_json::from_value(observations[4].1.clone()).unwrap();
    let warning = reopened.status.rules_cache.warning.clone().unwrap();
    assert!(!warning.is_empty() && reopened.status.rules_cache.loaded_at.is_some());
    assert_eq!(reopened.status.warnings, vec![warning.clone()]);
    assert_eq!(reopened.view_model.warnings, vec![warning]);
    let mut normalized = observations[4].1.clone();
    normalized["rules_cache"]["warning"] = observations[2].1["rules_cache"]["warning"].clone();
    normalized["rules_cache"]["loaded_at"] = observations[2].1["rules_cache"]["loaded_at"].clone();
    normalized["warnings"] = observations[2].1["warnings"].clone();
    normalized["view_model"]["warnings"] = observations[2].1["view_model"]["warnings"].clone();
    assert_eq!(normalized, observations[2].1);
}
