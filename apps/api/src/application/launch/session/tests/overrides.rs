use super::*;
use crate::guardian::GuardianFactId;

#[cfg(unix)]
#[tokio::test]
async fn custom_external_java_excludes_unused_managed_runtime_from_tier_zero() {
    let fixture = TestFixture::new("custom-external-java-tier-zero-runtime-scope");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let managed_root = fixture
        .state
        .managed_runtime_cache()
        .component_root("java-runtime-delta")
        .expect("managed runtime root");
    let managed_java = managed_runtime_java_path(&managed_root);
    fixture.activate_expected_version_inventory(
        &instance_id,
        "1.21.1",
        Some(b"client jar".len() as u64),
        [],
    );
    fs::remove_file(managed_java).expect("remove unused managed java");
    let external_java = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| instance.java_path = external_java);

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare custom external preflight");

    assert!(preflight.readiness.launchable);
    assert_ne!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    assert!(!preflight.guardian_facts.iter().any(|fact| {
        matches!(
            fact.id,
            GuardianFactId::ArtifactMissing | GuardianFactId::ArtifactSizeDrift
        )
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn direct_launch_receipt_keeps_preparation_retries_to_one_probe_spawn() {
    let fixture = TestFixture::new("direct-java-receipt-one-spawn");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let (java_path, count_file) = write_counted_probe_java(&fixture, "java21");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");
    assert_eq!(read_probe_count(&count_file), 1);
    let receipt = prepared
        .task
        .java_probe_receipt
        .as_ref()
        .expect("flow-local receipt");

    for retry_count in [0, 1] {
        let attempt = axial_launcher::service::AttemptOverrides {
            retry_count,
            ..Default::default()
        };
        let core_prepared = axial_launcher::prepare_launch_attempt_with_events(
            fixture.state.managed_runtime_cache(),
            &prepared.task.intent,
            &attempt,
            Some(receipt),
            |_| {},
        )
        .await
        .expect("receipt-backed preparation");
        assert_eq!(core_prepared.metrics.java_probe_count, 0);
        assert_eq!(core_prepared.metrics.java_probe_source, "receipt");
    }
    assert_eq!(read_probe_count(&count_file), 1);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "controlled wall-time evidence; run explicitly with --nocapture"]
async fn measure_override_preflight_receipt_wall_time() {
    const WARMUP_PAIRS: usize = 1;
    const TRIAL_PAIRS: usize = 6;

    for warmup in 0..WARMUP_PAIRS {
        run_override_preflight_measurement(OverridePreflightMeasurement::Baseline, warmup).await;
        run_override_preflight_measurement(OverridePreflightMeasurement::Current, warmup).await;
    }

    let mut baseline = Vec::with_capacity(TRIAL_PAIRS);
    let mut current = Vec::with_capacity(TRIAL_PAIRS);
    for trial in 0..TRIAL_PAIRS {
        let order = if trial % 2 == 0 {
            [
                OverridePreflightMeasurement::Baseline,
                OverridePreflightMeasurement::Current,
            ]
        } else {
            [
                OverridePreflightMeasurement::Current,
                OverridePreflightMeasurement::Baseline,
            ]
        };
        for measurement in order {
            let elapsed = run_override_preflight_measurement(measurement, trial).await;
            eprintln!(
                "override_preflight_wall_time trial={trial} shape={} elapsed_ms={:.3}",
                measurement.label(),
                elapsed.as_secs_f64() * 1_000.0
            );
            match measurement {
                OverridePreflightMeasurement::Baseline => baseline.push(elapsed),
                OverridePreflightMeasurement::Current => current.push(elapsed),
            }
        }
    }

    eprintln!(
        "override_preflight_wall_time median shape=baseline elapsed_ms={:.3} trials={TRIAL_PAIRS}",
        median_duration(&baseline).as_secs_f64() * 1_000.0
    );
    eprintln!(
        "override_preflight_wall_time median shape=current elapsed_ms={:.3} trials={TRIAL_PAIRS}",
        median_duration(&current).as_secs_f64() * 1_000.0
    );
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum OverridePreflightMeasurement {
    Baseline,
    Current,
}

#[cfg(unix)]
impl OverridePreflightMeasurement {
    fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Current => "current",
        }
    }
}

#[cfg(unix)]
async fn run_override_preflight_measurement(
    measurement: OverridePreflightMeasurement,
    trial: usize,
) -> std::time::Duration {
    let fixture = TestFixture::new(&format!(
        "override-preflight-measurement-{}-{trial}",
        measurement.label()
    ));
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let (java_path, count_file) = write_slow_counted_probe_java(&fixture, "java21");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let started_at = std::time::Instant::now();
    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare measured launch session");
    assert_eq!(read_probe_count(&count_file), 1);

    let receipt = match measurement {
        OverridePreflightMeasurement::Baseline => None,
        OverridePreflightMeasurement::Current => Some(
            prepared
                .task
                .java_probe_receipt
                .as_ref()
                .expect("measured flow-local receipt"),
        ),
    };
    let core_prepared = axial_launcher::prepare_launch_attempt_with_events(
        fixture.state.managed_runtime_cache(),
        &prepared.task.intent,
        &axial_launcher::service::AttemptOverrides::default(),
        receipt,
        |_| {},
    )
    .await
    .expect("measured core launch preparation");
    let elapsed = started_at.elapsed();

    match measurement {
        OverridePreflightMeasurement::Baseline => {
            assert_eq!(core_prepared.metrics.java_probe_count, 1);
            assert_eq!(core_prepared.metrics.java_probe_source, "fresh");
            assert_eq!(read_probe_count(&count_file), 2);
        }
        OverridePreflightMeasurement::Current => {
            assert_eq!(core_prepared.metrics.java_probe_count, 0);
            assert_eq!(core_prepared.metrics.java_probe_source, "receipt");
            assert_eq!(read_probe_count(&count_file), 1);
        }
    }

    drop(core_prepared);
    drop(prepared);
    drop(fixture);
    elapsed
}

#[cfg(unix)]
fn median_duration(samples: &[std::time::Duration]) -> std::time::Duration {
    assert!(!samples.is_empty(), "median requires at least one sample");
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let middle = ordered.len() / 2;
    if ordered.len().is_multiple_of(2) {
        (ordered[middle - 1] + ordered[middle]) / 2
    } else {
        ordered[middle]
    }
}

#[cfg(unix)]
#[tokio::test]
async fn standalone_preflight_does_not_cache_success_for_later_launch() {
    let fixture = TestFixture::new("standalone-java-success-not-cached");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let (java_path, count_file) = write_counted_probe_java(&fixture, "java21");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("standalone preflight");
    assert_eq!(read_probe_count(&count_file), 1);
    assert!(
        !serde_json::to_string(&preflight)
            .expect("preflight json")
            .contains("receipt")
    );

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("launch preflight");
    assert!(prepared.task.java_probe_receipt.is_some());
    assert_eq!(read_probe_count(&count_file), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_repreflight_shape_reuses_the_flow_receipt() {
    let fixture = TestFixture::new("runtime-repreflight-java-receipt");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let (java_path, count_file) = write_counted_probe_java(&fixture, "java21");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });
    let instance = fixture
        .state
        .instances()
        .get(&instance_id)
        .expect("instance");
    let config = fixture.state.config().current();
    let game_dir = fixture.state.instances().game_dir(&instance_id);
    let producer = fixture
        .state
        .try_claim_producer()
        .expect("claim preflight producer");

    let mut initial = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        None,
    )
    .await;
    let receipt = initial.java_probe_receipt.take().expect("initial receipt");
    let rebuilt = build_launch_preflight_facts(
        &fixture.state,
        &producer,
        LaunchPreflightBuild {
            instance_lifecycle: &fixture.state.acquire_instance_lifecycle(&instance.id).await,
            instance: &instance,
            config: &config,
            library_dir: &fixture.paths.library_dir,
            game_dir: &game_dir,
            requested_max_memory_mb: None,
            requested_min_memory_mb: None,
        },
        Some(receipt),
    )
    .await;

    assert!(rebuilt.java_probe_receipt.is_some());
    assert_eq!(read_probe_count(&count_file), 1);
}

#[tokio::test]
async fn launch_preflight_custom_override_warns_with_bounded_override_payload() {
    let fixture = TestFixture::new("preflight-custom-bounded");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path.clone();
        instance.extra_jvm_args = "-Dtoken=secret-token -XX:+UseZGC".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.status, "ready");
    assert_eq!(preflight.mode, GuardianMode::Custom);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Warned
    );
    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Instance)
    );
    assert_eq!(
        preflight.overrides.raw_jvm_args.origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(preflight.guardian.guidance().iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
    assert!(preflight.guardian.guidance().iter().any(|detail| detail
        == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."));

    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
    assert!(!payload.contains("-Dtoken"));
    assert!(!payload.contains("secret-token"));
    assert!(!payload.contains("requested_java"));
    assert!(!payload.contains("requested_preset"));
    assert!(!payload.contains("java_path"));
    assert!(!payload.contains("command"));
    assert!(!payload.contains("username"));
    for reason in &preflight.readiness.reasons {
        assert!(
            !reason
                .message
                .contains(&fixture.root.to_string_lossy().to_string())
        );
        assert!(!reason.message.contains("secret-token"));
    }
}

#[tokio::test]
async fn launch_preflight_bad_custom_java_override_blocks_with_guardian_fact() {
    let fixture = TestFixture::new("preflight-bad-custom-java-block");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert!(!preflight.readiness.launchable);
    assert_readiness_reason(&preflight, LaunchReadinessReasonId::JavaOverrideMissing);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Blocked
    );
    let fact = guardian_fact(&preflight, "java_override_missing");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(fact.ownership, OwnershipClass::UserOwned);
    assert_eq!(
        fact.target.as_ref().map(|target| target.id.as_str()),
        Some("instance_java_override")
    );
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian blocked launch because the selected Java override is unavailable."
    }));

    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    assert!(!payload.contains("/Users/SecretUser"));
    assert!(!payload.contains("manual/bin/java"));
}

#[tokio::test]
async fn prepare_launch_session_rejects_bad_custom_java_override_with_guardian_block() {
    let fixture = TestFixture::new("prepare-bad-custom-java-block");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "/Users/SecretUser/.jdks/manual/bin/java".to_string();
    });

    let error = match prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id: instance_id.clone(),
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    {
        Ok(_) => panic!("bad custom Java override should not queue"),
        Err(error) => error,
    };

    assert_eq!(error.0, StatusCode::PRECONDITION_FAILED);
    assert_eq!(error.1.0["readiness"]["launchable"], false);
    assert_eq!(
        error.1.0["readiness"]["reasons"][0]["id"],
        "java_override_missing"
    );
    assert_eq!(error.1.0["guardian"]["decision"], "blocked");
    assert!(
        error.1.0["guardian"]["details"]
            .as_array()
            .is_some_and(|details| details.iter().any(|detail| detail.as_str()
                == Some(
                    "Guardian blocked launch because the selected Java override is unavailable."
                )))
    );
    assert_eq!(fixture.state.sessions().active_session_count().await, 0);
    assert!(
        !fixture
            .state
            .sessions()
            .has_active_instance(&instance_id)
            .await
    );
    let payload = error.1.0.to_string();
    assert!(!payload.contains("/Users/SecretUser"));
    assert!(!payload.contains("manual/bin/java"));
}

#[tokio::test]
async fn launch_preflight_malformed_jvm_args_exposes_redacted_guardian_fact() {
    let fixture = TestFixture::new("preflight-malformed-jvm-fact");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args =
            r#"-Xmx2G "unterminated C:\Users\Alice\.jdks\java.exe"#.to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.guidance().iter().any(|detail| {
        detail == "Guardian removed malformed explicit JVM args for this launch."
    }));
    let fact = preflight
        .guardian_facts
        .iter()
        .find(|fact| fact.id.as_str() == "jvm_args_parse_failed")
        .expect("jvm parse fact");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
    assert_eq!(
        fact.target.as_ref().map(|target| target.id.as_str()),
        Some("explicit_jvm_args")
    );
    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    let lower = payload.to_ascii_lowercase();
    assert!(!lower.contains("alice"));
    assert!(!lower.contains(".jdks"));
    assert!(!lower.contains("-xmx"));
    assert!(!lower.contains("unterminated"));
}

#[tokio::test]
async fn launch_preflight_unsupported_jvm_gc_flags_exposes_guardian_fact() {
    let fixture = TestFixture::new("preflight-unsupported-jvm-fact");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args = "-XX:+UseZGC -Dtoken=secret-token".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.guidance().iter().any(|detail| {
        detail == "Guardian removed unsupported explicit JVM args for this launch."
    }));
    let fact = preflight
        .guardian_facts
        .iter()
        .find(|fact| fact.id.as_str() == "jvm_arg_unsupported_gc")
        .expect("unsupported jvm fact");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
    assert_eq!(
        fact.target.as_ref().map(|target| target.id.as_str()),
        Some("explicit_jvm_args")
    );

    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    let lower = payload.to_ascii_lowercase();
    assert!(!lower.contains("-xx:+usezgc"));
    assert!(!lower.contains("-dtoken"));
    assert!(!lower.contains("secret-token"));
}

#[tokio::test]
async fn unsafe_jvm_override_families_are_guardian_stripped_in_managed_mode() {
    let fixture = TestFixture::new("preflight-unsafe-jvm-override-managed");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args =
            "-cp /Users/Alice/secret.jar --class-path /Users/Alice/other.jar \
             -Djava.library.path=/Users/Alice/native -javaagent:/Users/Alice/agent.jar \
             -Dtoken=secret-token"
                .to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    for expected in [
        "jvm_arg_unsafe_classpath_override",
        "jvm_arg_unsafe_native_path_override",
        "jvm_arg_agent_override",
    ] {
        let fact = guardian_fact(&preflight, expected);
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("explicit_jvm_args")
        );
    }
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian removed explicit JVM args that override launcher-owned settings for this launch."
    }));
    assert!(preflight.guardian.guidance().iter().any(|detail| {
        detail == "Remove memory, classpath, native-path, or agent overrides from saved JVM args before re-enabling them."
    }));

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(prepared.task.intent.extra_jvm_args.is_empty());

    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    assert_unsafe_jvm_override_payload_is_redacted(&payload);
    let guardian_payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert_unsafe_jvm_override_payload_is_redacted(&guardian_payload);
}

#[tokio::test]
async fn unsafe_jvm_override_families_are_preserved_but_warned_in_custom_mode() {
    let fixture = TestFixture::new("preflight-unsafe-jvm-override-custom");
    fixture.set_guardian_mode("custom");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args =
            "-cp /Users/Alice/secret.jar --class-path /Users/Alice/other.jar \
             -Djava.library.path=/Users/Alice/native -javaagent:/Users/Alice/agent.jar \
             -Dtoken=secret-token"
                .to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.mode, GuardianMode::Custom);
    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Warned
    );
    assert_eq!(
        preflight.overrides.raw_jvm_args.origin,
        Some(OverrideOrigin::Instance)
    );
    for expected in [
        "jvm_arg_unsafe_classpath_override",
        "jvm_arg_unsafe_native_path_override",
        "jvm_arg_agent_override",
    ] {
        let fact = guardian_fact(&preflight, expected);
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);
    }
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly."
    }));
    assert!(preflight.guardian.guidance().iter().any(|detail| {
        detail == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable."
    }));
    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    assert_unsafe_jvm_override_payload_is_redacted(&payload);

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision(),
        GuardianSummaryDecision::Warned
    );
    assert_eq!(
        prepared.task.intent.extra_jvm_args,
        vec![
            "-cp",
            "/Users/Alice/secret.jar",
            "--class-path",
            "/Users/Alice/other.jar",
            "-Djava.library.path=/Users/Alice/native",
            "-javaagent:/Users/Alice/agent.jar",
            "-Dtoken=secret-token",
        ]
    );
    assert_eq!(
        prepared.task.intent.guardian.raw_jvm_args_origin,
        Some(OverrideOrigin::Instance)
    );
    let guardian_payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert_unsafe_jvm_override_payload_is_redacted(&guardian_payload);
}

#[tokio::test]
async fn launch_preflight_undefined_java_override_exposes_guardian_fact() {
    let fixture = TestFixture::new("preflight-undefined-java-fact");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "undefined".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(preflight.guardian.guidance().iter().any(|detail| {
        detail == "Guardian will ignore the unavailable Java override and use managed Java for this launch."
    }));
    let fact = preflight
        .guardian_facts
        .iter()
        .find(|fact| fact.id.as_str() == "java_override_undefined_sentinel")
        .expect("java sentinel fact");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(
        fact.target.as_ref().map(|target| target.id.as_str()),
        Some("instance_java_override")
    );
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "sentinel" && field.value == "undefined" })
    );
}

#[tokio::test]
async fn launch_preflight_null_java_override_exposes_guardian_fact() {
    let fixture = TestFixture::new("preflight-null-java-fact");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "null".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    let fact = preflight
        .guardian_facts
        .iter()
        .find(|fact| fact.id.as_str() == "java_override_undefined_sentinel")
        .expect("java sentinel fact");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "sentinel" && field.value == "null" })
    );
}

#[tokio::test]
async fn prepare_launch_session_uses_managed_java_for_undefined_java_override() {
    let fixture = TestFixture::new("prepare-undefined-java-managed-fallback");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "undefined".to_string();
    });

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert_eq!(prepared.task.intent.requested_java, "");
    assert!(prepared.task.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the unavailable Java override and use managed Java for this launch."
    }));
    let payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert!(!payload.contains("undefined"));
}

#[tokio::test]
async fn launch_preflight_missing_managed_java_override_falls_back_without_raw_path() {
    let fixture = TestFixture::new("preflight-missing-managed-java-fallback");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "/Users/SecretUser/.jdks/missing/bin/java".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    let fact = guardian_fact(&preflight, "java_override_missing");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(fact.ownership, OwnershipClass::UserOwned);
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the unavailable Java override and use managed Java for this launch."
    }));

    let payload = serde_json::to_string(&preflight).expect("serialize preflight");
    assert!(!payload.contains("/Users/SecretUser"));
    assert!(!payload.contains("missing/bin/java"));
}

#[tokio::test]
async fn prepare_launch_session_uses_managed_java_for_missing_managed_java_override() {
    let fixture = TestFixture::new("prepare-missing-managed-java-fallback");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "/Users/SecretUser/.jdks/missing/bin/java".to_string();
    });

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert_eq!(prepared.task.intent.requested_java, "");
    assert!(prepared.task.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the unavailable Java override and use managed Java for this launch."
    }));
    let payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert!(!payload.contains("/Users/SecretUser"));
    assert!(!payload.contains("missing/bin/java"));
}

#[tokio::test]
async fn launch_preflight_conflicting_memory_jvm_args_are_guardian_stripped() {
    let fixture = TestFixture::new("preflight-memory-jvm-stripped");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args = "-Xmx8G -Dtoken=secret-token".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian removed explicit JVM args that override launcher-owned settings for this launch."
    }));
    let fact = guardian_fact(&preflight, "jvm_arg_memory_conflict");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Jvm);

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(prepared.task.intent.extra_jvm_args.is_empty());
    let payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert!(!payload.to_ascii_lowercase().contains("-xmx"));
    assert!(!payload.contains("secret-token"));
}

#[tokio::test]
async fn launch_preflight_blank_instance_java_override_uses_global_override() {
    let fixture = TestFixture::new("preflight-empty-java-uses-global");
    let global_java = fixture.write_manual_java_override();
    fixture.set_global_java_override(&global_java);
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "   ".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Global)
    );
    assert!(!preflight.guardian_facts.iter().any(|fact| {
        fact.id.as_str() == "java_override_empty"
            && fact
                .target
                .as_ref()
                .is_some_and(|target| target.id == "instance_java_override")
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn launch_preflight_wrong_java_major_override_falls_back_with_guardian_fact() {
    let fixture = TestFixture::new("preflight-wrong-java-major");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = write_probe_java(
        &fixture,
        "java17",
        r#"openjdk version "17.0.10" 2024-01-16"#,
        true,
    );
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the incompatible Java override and use managed Java for this launch."
    }));
    let fact = guardian_fact(&preflight, "java_major_mismatch");
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "required_major" && field.value == "21" })
    );
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "actual_major" && field.value == "17" })
    );

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(prepared.task.intent.requested_java, "");
    let payload = serde_json::to_string(&preflight).expect("preflight json");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn launch_preflight_probe_failing_java_override_falls_back_without_raw_path() {
    let fixture = TestFixture::new("preflight-java-probe-failed");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = write_spawn_failing_java(&fixture, "java-probe-fails");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the Java override that failed probing and use managed Java for this launch."
    }));
    let fact = guardian_fact(&preflight, "java_probe_failed");
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "probe_failure" && field.value == "spawn_failed" })
    );

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(prepared.task.intent.requested_java, "");
    let payload = serde_json::to_string(&preflight).expect("preflight json");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[cfg(unix)]
#[tokio::test]
async fn launch_preflight_old_java8_update_falls_back_with_guardian_fact() {
    let fixture = TestFixture::new("preflight-old-java8-update");
    fixture.write_ready_install_with_java("1.8.9", "jre-legacy", 8);
    let instance_id = fixture.add_instance("Legacy", "1.8.9");
    let java_path = write_probe_java(
        &fixture,
        "java8u311",
        r#"openjdk version "1.8.0_311""#,
        true,
    );
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id.clone())
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.guardian.decision(),
        GuardianSummaryDecision::Intervened
    );
    assert!(preflight.guardian.details().iter().any(|detail| {
        detail == "Guardian will ignore the outdated Java override and use managed Java for this launch."
    }));
    let fact = guardian_fact(&preflight, "java_update_too_old");
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "required_min_update" && field.value == "312" })
    );
    assert!(
        fact.fields
            .iter()
            .any(|field| { field.key == "actual_update" && field.value == "311" })
    );

    let prepared = prepare_launch_session(
        &fixture.state,
        LaunchRequest {
            instance_id,
            username: None,
            max_memory_mb: None,
            min_memory_mb: None,
            client_started_at_ms: None,
        },
    )
    .await
    .expect("prepare launch session");

    assert_eq!(prepared.task.intent.requested_java, "");
    let payload = serde_json::to_string(&preflight).expect("preflight json");
    assert!(!payload.contains(&fixture.root.to_string_lossy().to_string()));
}

#[cfg(unix)]
fn write_probe_java(
    fixture: &TestFixture,
    name: &str,
    probe_output: &str,
    executable: bool,
) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = fixture.root.join(name).join("bin");
    fs::create_dir_all(&bin_dir).expect("fake java bin");
    let java_path = bin_dir.join("java");
    fs::write(
        &java_path,
        format!("#!/bin/sh\ncat >&2 <<'AXIAL_JAVA_PROBE'\n{probe_output}\nAXIAL_JAVA_PROBE\n"),
    )
    .expect("fake java script");
    if executable {
        let mut permissions = fs::metadata(&java_path)
            .expect("fake java metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&java_path, permissions).expect("fake java executable");
    }
    java_path.to_string_lossy().to_string()
}

#[cfg(unix)]
fn write_counted_probe_java(fixture: &TestFixture, name: &str) -> (String, PathBuf) {
    write_counted_probe_java_with_delay(fixture, name, None)
}

#[cfg(unix)]
fn write_slow_counted_probe_java(fixture: &TestFixture, name: &str) -> (String, PathBuf) {
    write_counted_probe_java_with_delay(fixture, name, Some("0.15"))
}

#[cfg(unix)]
fn write_counted_probe_java_with_delay(
    fixture: &TestFixture,
    name: &str,
    delay_seconds: Option<&str>,
) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = fixture.root.join(name).join("bin");
    fs::create_dir_all(&bin_dir).expect("fake java bin");
    let java_path = bin_dir.join("java");
    let count_file = fixture.root.join(format!("{name}-probe-count"));
    fs::write(&count_file, b"0").expect("probe count");
    fs::write(
        &java_path,
        format!(
            "#!/bin/sh\ncount=$(cat '{}')\necho $((count + 1)) > '{}'\n{}echo 'openjdk version \"21.0.3\"' >&2\n",
            count_file.display(),
            count_file.display(),
            delay_seconds
                .map(|seconds| format!("sleep {seconds}\n"))
                .unwrap_or_default()
        ),
    )
    .expect("counted fake java script");
    let mut permissions = fs::metadata(&java_path)
        .expect("fake java metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&java_path, permissions).expect("fake java executable");
    (java_path.to_string_lossy().to_string(), count_file)
}

#[cfg(unix)]
fn read_probe_count(path: &Path) -> u32 {
    fs::read_to_string(path)
        .expect("probe count")
        .trim()
        .parse()
        .expect("numeric probe count")
}

#[cfg(unix)]
fn write_spawn_failing_java(fixture: &TestFixture, name: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = fixture.root.join(name).join("bin");
    fs::create_dir_all(&bin_dir).expect("fake java bin");
    let java_path = bin_dir.join("java");
    fs::write(&java_path, "#!/axial/missing/probe/interpreter\nexit 1\n")
        .expect("fake java script");
    let mut permissions = fs::metadata(&java_path)
        .expect("fake java metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&java_path, permissions).expect("fake java executable");
    java_path.to_string_lossy().to_string()
}

fn assert_unsafe_jvm_override_payload_is_redacted(payload: &str) {
    let lower = payload.to_ascii_lowercase();
    for sensitive in [
        "alice",
        "secret.jar",
        "other.jar",
        "/users/",
        "java.library.path",
        "javaagent",
        "agent.jar",
        "-cp",
        "--class-path",
        "-dtoken",
        "secret-token",
    ] {
        assert!(
            !lower.contains(sensitive),
            "public launch payload exposed sensitive JVM override material: {sensitive}"
        );
    }
}
