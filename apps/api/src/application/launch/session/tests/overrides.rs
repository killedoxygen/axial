use super::*;

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
    assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Instance)
    );
    assert_eq!(
        preflight.overrides.raw_jvm_args.origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(preflight.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
    assert!(preflight.guardian.guidance.iter().any(|detail| detail
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
    assert_eq!(preflight.guardian.decision, GuardianDecision::Blocked);
    let fact = guardian_fact(&preflight, "java_override_missing");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(fact.ownership, OwnershipClass::UserOwned);
    assert_eq!(
        fact.target.as_ref().map(|target| target.id.as_str()),
        Some("instance_java_override")
    );
    assert!(preflight.guardian.details.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.guidance.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.guidance.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
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
    assert!(preflight.guardian.details.iter().any(|detail| {
        detail == "Guardian removed explicit JVM args that override launcher-owned settings for this launch."
    }));
    assert!(preflight.guardian.guidance.iter().any(|detail| {
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
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
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
    assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
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
    assert!(preflight.guardian.details.iter().any(|detail| {
        detail == "Guardian detected JVM arguments that override launcher-owned runtime settings. Remove them if startup fails or behaves unexpectedly."
    }));
    assert!(preflight.guardian.guidance.iter().any(|detail| {
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

    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(preflight.guardian.guidance.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
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
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
    );
    assert_eq!(prepared.task.intent.requested_java, "");
    assert!(prepared.task.guardian.details.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    let fact = guardian_fact(&preflight, "java_override_missing");
    assert_eq!(fact.domain, crate::guardian::GuardianDomain::Runtime);
    assert_eq!(fact.ownership, OwnershipClass::UserOwned);
    assert!(preflight.guardian.details.iter().any(|detail| {
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
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
    );
    assert_eq!(prepared.task.intent.requested_java, "");
    assert!(prepared.task.guardian.details.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.details.iter().any(|detail| {
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
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
    );
    assert!(prepared.task.intent.extra_jvm_args.is_empty());
    let payload = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert!(!payload.to_ascii_lowercase().contains("-xmx"));
    assert!(!payload.contains("secret-token"));
}

#[tokio::test]
async fn launch_preflight_blank_explicit_java_override_exposes_guardian_fact() {
    let fixture = TestFixture::new("preflight-empty-java-fact");
    fixture.set_global_java_override("/opt/java/bin/java");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = "   ".to_string();
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
    assert_eq!(
        preflight.overrides.java.origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(preflight.guardian_facts.iter().any(|fact| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.details.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.details.iter().any(|detail| {
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

    assert_eq!(preflight.guardian.decision, GuardianDecision::Intervened);
    assert!(preflight.guardian.details.iter().any(|detail| {
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
        format!("#!/bin/sh\ncat >&2 <<'CROOPOR_JAVA_PROBE'\n{probe_output}\nCROOPOR_JAVA_PROBE\n"),
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
fn write_spawn_failing_java(fixture: &TestFixture, name: &str) -> String {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = fixture.root.join(name).join("bin");
    fs::create_dir_all(&bin_dir).expect("fake java bin");
    let java_path = bin_dir.join("java");
    fs::write(&java_path, "#!/croopor/missing/probe/interpreter\nexit 1\n")
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
