use super::*;

#[tokio::test]
async fn launch_preflight_memory_clamp_warning_is_reflected() {
    let fixture = TestFixture::new("preflight-memory-clamp");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.max_memory_mb = 1024;
        instance.min_memory_mb = 2048;
    });

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(preflight.memory.max_memory_mb, 1024);
    assert_eq!(preflight.memory.min_memory_mb, 1024);
    assert!(preflight.memory.min_clamped);
    assert_eq!(preflight.guardian.decision, GuardianDecision::Warned);
    assert_has_memory_clamp_warning(&preflight.guardian);
}

#[tokio::test]
async fn launch_preflight_resource_warning_path_is_reflected() {
    let fixture = TestFixture::new("preflight-resource-warning");
    fixture.write_ready_install("1.21.1");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let preflight = prepare_launch_preflight(&fixture.state, instance_id)
        .await
        .expect("prepare preflight");

    assert_eq!(
        preflight.resource_budget.active_session_count,
        fixture.state.sessions().active_session_count().await
    );
    assert_eq!(
        preflight.resource_budget.active_install_count,
        fixture.state.installs().active_install_count().await
    );
    assert_eq!(
        preflight.resource_budget.requested_memory_mb,
        Some(preflight.memory.max_memory_mb)
    );
}

#[tokio::test]
async fn custom_mode_with_java_override_warns_before_queue() {
    let fixture = TestFixture::new("prepare-custom-java-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path.clone();
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_eq!(prepared.task.intent.requested_java, java_path);
    assert_eq!(
        prepared.task.intent.guardian.java_override_origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Switch Guardian back to Managed if you want Axial to adjust unsafe choices."));
}

#[tokio::test]
async fn custom_mode_with_raw_jvm_args_warns_before_queue() {
    let fixture = TestFixture::new("prepare-custom-jvm-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args = "-XX:+UseZGC -Ddemo=true".to_string();
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_eq!(
        prepared.task.intent.extra_jvm_args,
        vec!["-XX:+UseZGC", "-Ddemo=true"]
    );
    assert_eq!(
        prepared.task.intent.guardian.raw_jvm_args_origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(
        prepared
            .task
            .guardian
            .guidance
            .iter()
            .any(|detail| detail
                == "Guardian Custom mode will keep explicit JVM args; remove them first if startup becomes unstable.")
    );
}

#[tokio::test]
async fn malformed_raw_jvm_args_are_stripped_before_queue_in_managed_mode() {
    let fixture = TestFixture::new("prepare-malformed-jvm-stripped");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.extra_jvm_args = r#"-Xmx2G "unterminated"#.to_string();
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
    );
    assert!(
        prepared
            .task
            .guardian
            .details
            .iter()
            .any(|detail| detail
                == "Guardian removed malformed explicit JVM args for this launch.")
    );
    assert!(prepared.task.intent.extra_jvm_args.is_empty());
    let guardian = serde_json::to_string(&prepared.task.guardian).expect("guardian json");
    assert!(!guardian.to_ascii_lowercase().contains("-xmx"));
    assert!(!guardian.contains("unterminated"));
}

#[tokio::test]
async fn custom_mode_with_instance_jvm_preset_warns_before_queue() {
    let fixture = TestFixture::new("prepare-custom-preset-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.jvm_preset = "graalvm".to_string();
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_eq!(prepared.task.intent.requested_preset, "graalvm");
    assert_eq!(
        prepared.task.intent.guardian.preset_override_origin,
        Some(OverrideOrigin::Instance)
    );
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected JVM preset for this launch."));
    assert!(prepared.task.guardian.details.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected JVM preset for this launch."));
}

#[tokio::test]
async fn custom_mode_with_global_jvm_preset_warns_before_queue() {
    let fixture = TestFixture::new("prepare-custom-global-preset-warning");
    fixture.set_guardian_mode("custom");
    fixture.set_global_jvm_preset("performance");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_eq!(prepared.task.intent.requested_preset, "performance");
    assert_eq!(
        prepared.task.intent.guardian.preset_override_origin,
        Some(OverrideOrigin::Global)
    );
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected JVM preset for this launch."));
}

#[tokio::test]
async fn managed_mode_with_manual_overrides_skips_custom_warning_at_queue_time() {
    let fixture = TestFixture::new("prepare-managed-overrides-no-custom-warning");
    fixture.set_guardian_mode("managed");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path.clone();
        instance.jvm_preset = "graalvm".to_string();
        instance.extra_jvm_args = "-XX:+UseZGC".to_string();
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert!(
        !prepared
            .task
            .guardian
            .guidance
            .iter()
            .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
    );
    assert!(
        !prepared
            .task
            .guardian
            .details
            .iter()
            .any(|detail| detail.starts_with("Guardian Custom mode will keep"))
    );
    assert_eq!(prepared.task.intent.requested_java, java_path);
    assert_eq!(prepared.task.intent.requested_preset, "graalvm");
    assert!(prepared.task.intent.extra_jvm_args.is_empty());
    assert_eq!(
        prepared.task.guardian.decision,
        GuardianDecision::Intervened
    );
    assert!(prepared.task.guardian.details.iter().any(|detail| {
        detail == "Guardian removed unsupported explicit JVM args for this launch."
    }));
}

#[tokio::test]
async fn instance_min_above_max_warns_and_clamps_intent_min_to_max() {
    let fixture = TestFixture::new("prepare-instance-memory-clamp-warning");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    fixture.update_instance(&instance_id, |instance| {
        instance.max_memory_mb = 1024;
        instance.min_memory_mb = 2048;
    });

    let prepared = fixture
        .prepare(instance_id.clone(), None)
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 1024);
    assert_eq!(prepared.task.intent.min_memory_mb, 1024);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_has_memory_clamp_warning(&prepared.task.guardian);
}

#[tokio::test]
async fn request_min_above_request_max_warns_for_api_callers() {
    let fixture = TestFixture::new("prepare-request-memory-clamp-warning");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let prepared = fixture
        .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 1024);
    assert_eq!(prepared.task.intent.min_memory_mb, 1024);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_has_memory_clamp_warning(&prepared.task.guardian);
}

#[tokio::test]
async fn normal_min_at_or_below_max_does_not_add_clamp_warning() {
    let fixture = TestFixture::new("prepare-no-memory-clamp-warning");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let prepared = fixture
        .prepare_with_memory(instance_id.clone(), Some(4096), Some(1024))
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 4096);
    assert_eq!(prepared.task.intent.min_memory_mb, 1024);
    assert_no_memory_clamp_warning(&prepared.task.guardian);
}

#[tokio::test]
async fn low_max_memory_warns_without_changing_intent_memory_values() {
    let fixture = TestFixture::new("prepare-low-max-memory-warning");
    let instance_id = fixture.add_instance("Survival", "1.21.1");

    let prepared = fixture
        .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 1024);
    assert_eq!(prepared.task.intent.min_memory_mb, 512);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
    assert_no_memory_clamp_warning(&prepared.task.guardian);
}

#[tokio::test]
async fn memory_clamp_warning_merges_with_custom_override_warning() {
    let fixture = TestFixture::new("prepare-memory-clamp-custom-merged-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let prepared = fixture
        .prepare_with_memory(instance_id.clone(), Some(1024), Some(2048))
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 1024);
    assert_eq!(prepared.task.intent.min_memory_mb, 1024);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_has_memory_clamp_warning(&prepared.task.guardian);
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
    assert!(prepared.task.guardian.details.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
}

#[tokio::test]
async fn low_memory_warning_merges_with_custom_override_warning() {
    let fixture = TestFixture::new("prepare-low-memory-custom-merged-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let prepared = fixture
        .prepare_with_memory(instance_id.clone(), Some(1024), Some(512))
        .await
        .expect("prepare launch session");

    assert_eq!(prepared.task.intent.max_memory_mb, 1024);
    assert_eq!(prepared.task.intent.min_memory_mb, 512);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    assert_has_low_memory_allocation_warning(&prepared.task.guardian, 1024);
    assert!(prepared.task.guardian.guidance.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
    assert!(prepared.task.guardian.details.iter().any(|detail| detail
        == "Guardian Custom mode will keep the selected Java override for this launch."));
}

#[tokio::test]
async fn memory_warning_and_custom_override_warning_merge_before_queue() {
    let fixture = TestFixture::new("prepare-merged-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });

    let prepared = fixture
        .prepare(instance_id.clone(), Some(i32::MAX))
        .await
        .expect("prepare launch session");

    let resource_budget = prepared
        .task
        .resource_budget
        .as_ref()
        .expect("resource budget snapshot");
    assert_eq!(resource_budget.active_session_count, 0);
    assert_eq!(resource_budget.active_install_count, 0);
    assert_eq!(resource_budget.active_memory_allocation_mb, 0);
    assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
    assert!(resource_budget.memory_pressure);
    assert!(!resource_budget.install_pressure);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    for expected in [
        "Launch memory budget is tight for the current active sessions.",
        "Guardian Custom mode will keep the selected Java override for this launch.",
        "Switch Guardian back to Managed if you want Axial to adjust unsafe choices.",
    ] {
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail == expected),
            "missing guidance: {expected}"
        );
        assert!(
            prepared
                .task
                .guardian
                .details
                .iter()
                .any(|detail| detail == expected),
            "missing detail: {expected}"
        );
    }
}

#[tokio::test]
async fn resource_budget_warnings_merge_with_existing_guardian_guidance_before_queue() {
    let fixture = TestFixture::new("prepare-resource-merged-warning");
    fixture.set_guardian_mode("custom");
    let instance_id = fixture.add_instance("Survival", "1.21.1");
    let java_path = fixture.write_manual_java_override();
    fixture.update_instance(&instance_id, |instance| {
        instance.java_path = java_path;
    });
    for index in 0..4 {
        fixture
            .add_active_launch(&format!("active-launch-{index}"), 1024)
            .await;
    }
    fixture.add_active_install("active-install").await;

    let prepared = fixture
        .prepare(instance_id.clone(), Some(i32::MAX))
        .await
        .expect("prepare launch session");

    let resource_budget = prepared
        .task
        .resource_budget
        .as_ref()
        .expect("resource budget snapshot");
    assert_eq!(resource_budget.active_session_count, 4);
    assert_eq!(resource_budget.active_install_count, 1);
    assert_eq!(resource_budget.active_memory_allocation_mb, 4096);
    assert_eq!(resource_budget.requested_memory_mb, Some(i32::MAX));
    assert!(resource_budget.memory_pressure);
    assert!(resource_budget.install_pressure);
    assert_eq!(prepared.task.guardian.decision, GuardianDecision::Warned);
    for expected in [
        "Launch memory budget is tight for the current active sessions.",
        "Multiple launches can saturate low-end CPUs; wait for another launch to finish if startup feels sluggish.",
        "Active install or download work may add pressure during startup.",
        "Guardian Custom mode will keep the selected Java override for this launch.",
    ] {
        assert!(
            prepared
                .task
                .guardian
                .guidance
                .iter()
                .any(|detail| detail == expected),
            "missing guidance: {expected}"
        );
    }
    assert!(
        prepared
            .task
            .guardian
            .guidance
            .iter()
            .any(|detail| detail.starts_with("Launch concurrency may be tight:"))
    );
}
