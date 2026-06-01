use super::{
    AttemptOverrides, HealingSummaryInput, LaunchIntent, LaunchPreparationError,
    LaunchPreparationMetrics, PreparedLaunchAttempt, build_healing_summary, infer_loader,
};
use crate::build::{VanillaLaunchRequest, plan_resolved_launch};
use crate::guardian::resolve_launch_preset;
use crate::jvm::{boot_throttle_args, gc_preset_args};
use crate::runtime::RuntimeSelection;
use crate::types::LaunchFailureClass;
use croopor_minecraft::{
    JavaRuntimeInfo, JavaVersion, RuntimeEnsureEvent, ensure_runtime_with_events, resolve_version,
};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPreparationEvent {
    EnsuringRuntime,
    DownloadingRuntime,
}

pub async fn prepare_launch_attempt(
    intent: &LaunchIntent,
    attempt: &AttemptOverrides,
) -> Result<PreparedLaunchAttempt, LaunchPreparationError> {
    prepare_launch_attempt_with_events(intent, attempt, |_| {}).await
}

pub async fn prepare_launch_attempt_with_events<F>(
    intent: &LaunchIntent,
    attempt: &AttemptOverrides,
    mut observer: F,
) -> Result<PreparedLaunchAttempt, LaunchPreparationError>
where
    F: FnMut(LaunchPreparationEvent),
{
    let started_at = Instant::now();
    let version_started_at = Instant::now();
    let version = resolve_version(&intent.library_dir, &intent.version_id).map_err(|error| {
        LaunchPreparationError {
            message: error.to_string(),
            failure_class: Some(LaunchFailureClass::Unknown),
            healing: None,
        }
    })?;
    let version_ms = version_started_at.elapsed().as_millis();
    let auth_mode = launch_auth_mode_for_context(intent);

    let runtime_started_at = Instant::now();
    observer(LaunchPreparationEvent::EnsuringRuntime);
    let ensured_runtime = ensure_runtime_with_events(
        &intent.library_dir,
        &version.java_version,
        &intent.requested_java,
        attempt.force_managed_runtime,
        |event| observer(launch_preparation_event_for_runtime_event(event)),
    )
    .await
    .map_err(|error| LaunchPreparationError {
        message: format!("resolve java: {error}"),
        failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
        healing: build_healing_summary(HealingSummaryInput {
            auth_mode,
            requested_java_path: &intent.requested_java,
            requested_preset: &intent.requested_preset,
            effective_java_path: None,
            effective_preset: None,
            fallback_applied: attempt.fallback_applied.as_deref(),
            retry_count: attempt.retry_count,
            failure_class: Some(LaunchFailureClass::JavaRuntimeMismatch),
        }),
    })?;
    let runtime_ms = runtime_started_at.elapsed().as_millis();

    if intent.guardian.has_java_override()
        && let Some(requested_runtime) = ensured_runtime.requested.as_ref()
        && let Err((class, message)) = super::validation::validate_requested_java_override(
            &intent.requested_java,
            &requested_runtime.info,
            version.java_version.major_version,
        )
    {
        return Err(LaunchPreparationError {
            message,
            failure_class: Some(class),
            healing: build_healing_summary(HealingSummaryInput {
                auth_mode,
                requested_java_path: &intent.requested_java,
                requested_preset: &intent.requested_preset,
                effective_java_path: Some(ensured_runtime.effective.java_path.as_str()),
                effective_preset: None,
                fallback_applied: attempt.fallback_applied.as_deref(),
                retry_count: attempt.retry_count,
                failure_class: Some(class),
            }),
        });
    }

    let mut runtime = runtime_selection_from_ensure(&intent.requested_java, ensured_runtime);
    sanitize_effective_runtime_major(&mut runtime, &version.java_version);

    let loader = infer_loader(&intent.version_id);
    let is_modded = loader != "vanilla" || !version.inherits_from.trim().is_empty();
    let mut guardian_interventions = Vec::new();
    let mut effective_preset = if let Some(preset_override) = attempt.preset_override.clone() {
        preset_override
    } else {
        let resolved = resolve_launch_preset(
            &intent.guardian,
            &intent.requested_preset,
            &intent.version_id,
            loader,
            is_modded,
            &runtime.effective_info,
        )
        .map_err(|(class, message)| LaunchPreparationError {
            message,
            failure_class: Some(class),
            healing: build_healing_summary(HealingSummaryInput {
                auth_mode,
                requested_java_path: &intent.requested_java,
                requested_preset: &intent.requested_preset,
                effective_java_path: Some(runtime.effective_path.as_str()),
                effective_preset: None,
                fallback_applied: attempt.fallback_applied.as_deref(),
                retry_count: attempt.retry_count,
                failure_class: Some(class),
            }),
        })?;
        if let Some(intervention) = resolved.intervention {
            guardian_interventions.push(intervention);
        }
        resolved.effective_preset
    };

    if intent.guardian.has_java_override()
        && let Err((class, message)) = super::validation::validate_manual_java_override(
            &intent.requested_java,
            &runtime,
            version.java_version.major_version,
        )
    {
        return Err(LaunchPreparationError {
            message,
            failure_class: Some(class),
            healing: build_healing_summary(HealingSummaryInput {
                auth_mode,
                requested_java_path: &intent.requested_java,
                requested_preset: &intent.requested_preset,
                effective_java_path: Some(runtime.effective_path.as_str()),
                effective_preset: Some(effective_preset.as_str()),
                fallback_applied: attempt.fallback_applied.as_deref(),
                retry_count: attempt.retry_count,
                failure_class: Some(class),
            }),
        });
    }
    let effective_extra_jvm_args = if attempt.ignore_extra_jvm_args {
        Vec::new()
    } else {
        intent.extra_jvm_args.clone()
    };
    if intent.guardian.has_raw_jvm_args()
        && let Err((class, message)) = super::validation::validate_manual_jvm_args(
            &effective_extra_jvm_args,
            &runtime.effective_info,
        )
    {
        return Err(LaunchPreparationError {
            message,
            failure_class: Some(class),
            healing: build_healing_summary(HealingSummaryInput {
                auth_mode,
                requested_java_path: &intent.requested_java,
                requested_preset: &intent.requested_preset,
                effective_java_path: Some(runtime.effective_path.as_str()),
                effective_preset: Some(effective_preset.as_str()),
                fallback_applied: attempt.fallback_applied.as_deref(),
                retry_count: attempt.retry_count,
                failure_class: Some(class),
            }),
        });
    }

    let healing = build_healing_summary(HealingSummaryInput {
        auth_mode,
        requested_java_path: &intent.requested_java,
        requested_preset: &intent.requested_preset,
        effective_java_path: Some(runtime.effective_path.as_str()),
        effective_preset: Some(effective_preset.as_str()),
        fallback_applied: attempt.fallback_applied.as_deref(),
        retry_count: attempt.retry_count,
        failure_class: None,
    });

    let mut extra_jvm_args = boot_throttle_args(runtime.effective_info.major);
    if !effective_preset.trim().is_empty() && !attempt.disable_custom_gc {
        extra_jvm_args.extend(gc_preset_args(
            &effective_preset,
            &runtime.effective_info,
            uses_low_impact_startup(&intent.performance_mode),
        ));
    } else if attempt.disable_custom_gc {
        effective_preset.clear();
    }
    extra_jvm_args.extend(effective_extra_jvm_args);

    let planning_started_at = Instant::now();
    let plan = plan_resolved_launch(
        &VanillaLaunchRequest {
            session_id: intent.session_id.clone(),
            mc_dir: intent.library_dir.clone(),
            version_id: intent.version_id.clone(),
            auth: intent.auth.clone(),
            runtime: runtime.clone(),
            game_dir: intent.game_dir.clone(),
            launcher_name: intent.launcher_name.clone(),
            launcher_version: intent.launcher_version.clone(),
            min_memory_mb: Some(intent.min_memory_mb),
            max_memory_mb: Some(intent.max_memory_mb),
            extra_jvm_args,
            resolution: intent.resolution,
        },
        version,
    )
    .map_err(|error| LaunchPreparationError {
        message: error.to_string(),
        failure_class: Some(LaunchFailureClass::Unknown),
        healing: healing.clone(),
    })?;
    let planning_ms = planning_started_at.elapsed().as_millis();

    Ok(PreparedLaunchAttempt {
        runtime,
        effective_preset,
        plan,
        healing,
        guardian_interventions,
        metrics: LaunchPreparationMetrics {
            version_ms,
            runtime_ms,
            planning_ms,
            total_ms: started_at.elapsed().as_millis(),
        },
    })
}

fn launch_preparation_event_for_runtime_event(event: RuntimeEnsureEvent) -> LaunchPreparationEvent {
    match event {
        RuntimeEnsureEvent::DownloadingManagedRuntime { .. } => {
            LaunchPreparationEvent::DownloadingRuntime
        }
    }
}

fn uses_low_impact_startup(performance_mode: &str) -> bool {
    !matches!(performance_mode.trim(), "custom")
}

fn launch_auth_mode_for_context(intent: &LaunchIntent) -> &'static str {
    if intent.auth.user_type == "msa" {
        "online"
    } else {
        "offline"
    }
}

fn runtime_selection_from_ensure(
    requested_java: &str,
    ensured: croopor_minecraft::RuntimeEnsureResult,
) -> RuntimeSelection {
    let selected = ensured
        .requested
        .clone()
        .unwrap_or_else(|| ensured.effective.clone());
    let selected_path = if requested_java.trim().is_empty() {
        String::new()
    } else {
        selected.java_path.clone()
    };
    let selected_info = if requested_java.trim().is_empty() {
        JavaRuntimeInfo {
            id: String::new(),
            major: 0,
            update: 0,
            distribution: "unknown".to_string(),
            path: String::new(),
        }
    } else {
        selected.info.clone()
    };

    RuntimeSelection {
        requested_path: requested_java.trim().to_string(),
        selected_path,
        selected_info,
        effective_path: ensured.effective.java_path.clone(),
        effective_info: ensured.effective.info.clone(),
        effective_source: ensured.effective.source.as_str().to_string(),
        bypassed_requested_runtime: ensured.bypassed_requested_runtime,
    }
}

pub fn sanitize_effective_runtime_major(
    runtime: &mut RuntimeSelection,
    java_version: &JavaVersion,
) {
    if runtime.effective_path.is_empty() {
        return;
    }
    if runtime.effective_info.major == 0 && java_version.major_version > 0 {
        runtime.effective_info.major = java_version.major_version as u32;
    }
}

// This no-spawn representative gate relies on a shell-script fake Java that
// std::process::Command can execute directly on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::build::LaunchAuthContext;
    use crate::guardian::{GuardianMode, LaunchGuardianContext, OverrideOrigin};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn prepare_representative_fabric_launch_plans_without_spawning_java() {
        let root = unique_temp_root("croopor-prepare-fabric-gate");
        let library_dir = root.join("library");
        let instance_root = root.join("instances");
        let fake_java = write_fake_java(&root);

        let targets = [
            RepresentativeTarget {
                minecraft_version: "1.16.5",
                loader_version: "0.16.10",
                java_major: 8,
                family: "E",
            },
            RepresentativeTarget {
                minecraft_version: "1.20.1",
                loader_version: "0.16.10",
                java_major: 17,
                family: "E",
            },
            RepresentativeTarget {
                minecraft_version: "1.21.1",
                loader_version: "0.16.10",
                java_major: 21,
                family: "F",
            },
        ];

        for target in targets {
            let version_id = write_fabric_version(&library_dir, target);
            let game_dir = instance_root.join(target.minecraft_version);
            fs::create_dir_all(&game_dir).expect("instance dir");

            let intent = LaunchIntent {
                session_id: format!("prepare-gate-{}", target.minecraft_version),
                library_dir: library_dir.clone(),
                instance_id: format!("fabric-{}", target.minecraft_version),
                version_id: version_id.clone(),
                username: "Player".to_string(),
                auth: LaunchAuthContext::offline("Player"),
                requested_java: fake_java.to_string_lossy().to_string(),
                requested_preset: String::new(),
                extra_jvm_args: Vec::new(),
                max_memory_mb: 6144,
                min_memory_mb: 1024,
                resolution: None,
                launcher_name: "croopor".to_string(),
                launcher_version: "test".to_string(),
                game_dir: Some(game_dir.clone()),
                guardian: LaunchGuardianContext {
                    mode: GuardianMode::Managed,
                    ..LaunchGuardianContext::default()
                },
                performance_mode: "managed".to_string(),
            };

            let prepared = prepare_launch_attempt(&intent, &AttemptOverrides::default())
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "prepare failed for {} Fabric {}: {}",
                        target.family, target.minecraft_version, error.message
                    )
                });

            let plan = prepared.plan;
            assert_eq!(
                plan.command.first().map(String::as_str),
                Some(fake_java.to_string_lossy().as_ref()),
                "{version_id} should launch through the fake Java override"
            );
            assert!(
                plan.jvm_args.iter().any(|arg| arg == "-Xmx6144M"),
                "{version_id} should preserve backend-selected max memory"
            );
            assert!(
                plan.jvm_args.iter().any(|arg| arg == "-Xms1024M"),
                "{version_id} should preserve backend-selected min memory"
            );
            assert_eq!(
                plan.game_dir, game_dir,
                "{version_id} should use the isolated instance game dir"
            );
            assert!(
                !plan.main_class.trim().is_empty()
                    && plan.command.iter().any(|arg| arg == &plan.main_class),
                "{version_id} should include a main class in the final command"
            );
            assert_eq!(
                plan.main_class, "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "{version_id} should plan the Fabric entrypoint"
            );
            assert_eq!(
                prepared.effective_preset,
                crate::jvm::PRESET_PERFORMANCE,
                "{version_id} should resolve the managed modded preset"
            );
            assert!(
                plan.jvm_args.iter().any(|arg| arg == "-XX:+UseG1GC"),
                "{version_id} should include managed-mode GC preset args"
            );
            assert!(
                plan.jvm_args
                    .iter()
                    .any(|arg| arg == "-XX:MaxGCPauseMillis=37"),
                "{version_id} should include the performance preset pause target"
            );
            assert!(
                !plan.jvm_args.iter().any(|arg| arg == "-XX:+AlwaysPreTouch"),
                "{version_id} should keep managed-mode low-impact startup behavior"
            );
            assert!(
                plan.classpath.contains("fabric-loader-0.16.10.jar"),
                "{version_id} should include the fabricated Fabric loader jar"
            );
            assert_eq!(
                plan.version.id, version_id,
                "{version_id} should prepare the representative Fabric version id"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_launch_attempt_uses_offline_auth_context_from_intent_username() {
        let root = unique_temp_root("croopor-prepare-auth-test");
        let library_dir = root.join("library");
        let game_dir = root.join("instances").join("auth-test");
        let fake_java = write_fake_java(&root);
        let version_id = "auth-test";
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(&game_dir).expect("game dir");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("client jar");
        write_version_json(
            &version_dir.join(format!("{version_id}.json")),
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "assetIndex": { "id": "auth-assets" },
                "arguments": {
                    "jvm": [],
                    "game": [
                        "--username",
                        "${auth_player_name}",
                        "--uuid",
                        "${auth_uuid}",
                        "--accessToken",
                        "${auth_access_token}",
                        "--userType",
                        "${user_type}"
                    ]
                },
                "libraries": []
            }),
        );

        let intent = LaunchIntent {
            session_id: "prepare-auth-test".to_string(),
            library_dir: library_dir.clone(),
            instance_id: "auth-test".to_string(),
            version_id: version_id.to_string(),
            username: "Player".to_string(),
            auth: LaunchAuthContext::offline("Player"),
            requested_java: fake_java.to_string_lossy().to_string(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 2048,
            min_memory_mb: 512,
            resolution: None,
            launcher_name: "croopor".to_string(),
            launcher_version: "test".to_string(),
            game_dir: Some(game_dir),
            guardian: LaunchGuardianContext {
                mode: GuardianMode::Managed,
                ..LaunchGuardianContext::default()
            },
            performance_mode: "managed".to_string(),
        };

        let prepared = prepare_launch_attempt(&intent, &AttemptOverrides::default())
            .await
            .expect("prepared launch");

        assert_arg_value(&prepared.plan.game_args, "--username", "Player");
        assert_arg_value(
            &prepared.plan.game_args,
            "--uuid",
            &croopor_minecraft::offline_uuid("Player"),
        );
        assert_arg_value(&prepared.plan.game_args, "--accessToken", "null");
        assert_arg_value(&prepared.plan.game_args, "--userType", "legacy");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn custom_explicit_unsupported_named_preset_fails_before_command_planning() {
        let root = unique_temp_root("croopor-prepare-custom-preset-block-test");
        let library_dir = root.join("library");
        let game_dir = root.join("instances").join("custom-preset-block-test");
        let fake_java = write_fake_openj9_java(&root);
        let version_id = "custom-preset-block-test";
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(&game_dir).expect("game dir");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("client jar");
        write_version_json(
            &version_dir.join(format!("{version_id}.json")),
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "assetIndex": { "id": "custom-preset-assets" },
                "arguments": {
                    "jvm": [],
                    "game": []
                },
                "libraries": []
            }),
        );

        let intent = LaunchIntent {
            session_id: "prepare-custom-preset-block-test".to_string(),
            library_dir: library_dir.clone(),
            instance_id: "custom-preset-block-test".to_string(),
            version_id: version_id.to_string(),
            username: "Player".to_string(),
            auth: LaunchAuthContext::offline("Player"),
            requested_java: fake_java.to_string_lossy().to_string(),
            requested_preset: crate::jvm::PRESET_SMOOTH.to_string(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 2048,
            min_memory_mb: 512,
            resolution: None,
            launcher_name: "croopor".to_string(),
            launcher_version: "test".to_string(),
            game_dir: Some(game_dir),
            guardian: LaunchGuardianContext {
                mode: GuardianMode::Custom,
                java_override_origin: Some(OverrideOrigin::Instance),
                preset_override_origin: Some(OverrideOrigin::Instance),
                raw_jvm_args_origin: None,
            },
            performance_mode: "managed".to_string(),
        };

        let error = prepare_launch_attempt(&intent, &AttemptOverrides::default())
            .await
            .expect_err("known-fatal custom preset should fail during preparation");

        assert_eq!(
            error.failure_class,
            Some(LaunchFailureClass::JvmUnsupportedOption)
        );
        assert!(error.message.contains("Smooth"));
        assert!(error.message.contains("HotSpot JVM tuning flags"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_launch_attempt_uses_explicit_online_auth_context() {
        let root = unique_temp_root("croopor-prepare-online-auth-test");
        let library_dir = root.join("library");
        let game_dir = root.join("instances").join("online-auth-test");
        let fake_java = write_fake_java(&root);
        let version_id = "online-auth-test";
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(&game_dir).expect("game dir");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("client jar");
        write_version_json(
            &version_dir.join(format!("{version_id}.json")),
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "assetIndex": { "id": "auth-assets" },
                "arguments": {
                    "jvm": [],
                    "game": [
                        "--username",
                        "${auth_player_name}",
                        "--uuid",
                        "${auth_uuid}",
                        "--accessToken",
                        "${auth_access_token}",
                        "--userType",
                        "${user_type}"
                    ]
                },
                "libraries": []
            }),
        );

        let intent = LaunchIntent {
            session_id: "prepare-online-auth-test".to_string(),
            library_dir: library_dir.clone(),
            instance_id: "online-auth-test".to_string(),
            version_id: version_id.to_string(),
            username: "OfflineName".to_string(),
            auth: LaunchAuthContext {
                player_name: "ProfileName".to_string(),
                uuid: "4f9c7f7d0b1245d9a5c2f03a8c120001".to_string(),
                access_token: "minecraft-access-token".to_string(),
                client_id: String::new(),
                xuid: String::new(),
                user_type: "msa".to_string(),
            },
            requested_java: fake_java.to_string_lossy().to_string(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 2048,
            min_memory_mb: 512,
            resolution: None,
            launcher_name: "croopor".to_string(),
            launcher_version: "test".to_string(),
            game_dir: Some(game_dir),
            guardian: LaunchGuardianContext {
                mode: GuardianMode::Managed,
                ..LaunchGuardianContext::default()
            },
            performance_mode: "managed".to_string(),
        };

        let prepared = prepare_launch_attempt(&intent, &AttemptOverrides::default())
            .await
            .expect("prepared launch");

        assert_arg_value(&prepared.plan.game_args, "--username", "ProfileName");
        assert_arg_value(
            &prepared.plan.game_args,
            "--uuid",
            "4f9c7f7d0b1245d9a5c2f03a8c120001",
        );
        assert_arg_value(
            &prepared.plan.game_args,
            "--accessToken",
            "minecraft-access-token",
        );
        assert_arg_value(&prepared.plan.game_args, "--userType", "msa");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_launch_attempt_with_events_observes_runtime_resolution() {
        let root = unique_temp_root("croopor-prepare-runtime-event-test");
        let library_dir = root.join("library");
        let game_dir = root.join("instances").join("runtime-event-test");
        let fake_java = write_fake_java(&root);
        let version_id = "runtime-event-test";
        let version_dir = library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::create_dir_all(&game_dir).expect("game dir");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("client jar");
        write_version_json(
            &version_dir.join(format!("{version_id}.json")),
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "javaVersion": {
                    "component": "java-runtime-delta",
                    "majorVersion": 21
                },
                "assetIndex": { "id": "runtime-event-assets" },
                "arguments": {
                    "jvm": [],
                    "game": []
                },
                "libraries": []
            }),
        );

        let intent = LaunchIntent {
            session_id: "prepare-runtime-event-test".to_string(),
            library_dir,
            instance_id: "runtime-event-test".to_string(),
            version_id: version_id.to_string(),
            username: "Player".to_string(),
            auth: LaunchAuthContext::offline("Player"),
            requested_java: fake_java.to_string_lossy().to_string(),
            requested_preset: String::new(),
            extra_jvm_args: Vec::new(),
            max_memory_mb: 2048,
            min_memory_mb: 512,
            resolution: None,
            launcher_name: "croopor".to_string(),
            launcher_version: "test".to_string(),
            game_dir: Some(game_dir),
            guardian: LaunchGuardianContext {
                mode: GuardianMode::Managed,
                ..LaunchGuardianContext::default()
            },
            performance_mode: "managed".to_string(),
        };
        let mut events = Vec::new();

        let prepared =
            prepare_launch_attempt_with_events(&intent, &AttemptOverrides::default(), |event| {
                events.push(event);
            })
            .await
            .expect("prepared launch");

        assert_eq!(prepared.runtime.effective_source, "override");
        assert_eq!(events, vec![LaunchPreparationEvent::EnsuringRuntime]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_download_event_maps_to_launch_preparation_download() {
        assert_eq!(
            launch_preparation_event_for_runtime_event(
                RuntimeEnsureEvent::DownloadingManagedRuntime {
                    component: "java-runtime-delta".to_string(),
                },
            ),
            LaunchPreparationEvent::DownloadingRuntime
        );
    }

    #[derive(Clone, Copy)]
    struct RepresentativeTarget {
        minecraft_version: &'static str,
        loader_version: &'static str,
        java_major: i32,
        family: &'static str,
    }

    fn unique_temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn write_fake_java(root: &Path) -> PathBuf {
        write_fake_java_with_probe(
            root,
            r#"#!/bin/sh
echo 'java.vendor = OpenJDK' >&2
echo 'java.vm.name = OpenJDK 64-Bit Server VM' >&2
echo 'openjdk version "21.0.3" 2024-04-16' >&2
"#,
        )
    }

    fn write_fake_openj9_java(root: &Path) -> PathBuf {
        write_fake_java_with_probe(
            root,
            r#"#!/bin/sh
echo 'java.vendor = IBM Corporation' >&2
echo 'java.vm.name = Eclipse OpenJ9 VM' >&2
echo 'openjdk version "21.0.3" 2024-04-16' >&2
"#,
        )
    }

    fn write_fake_java_with_probe(root: &Path, probe_output_script: &str) -> PathBuf {
        let bin_dir = root.join("fake-java").join("bin");
        fs::create_dir_all(&bin_dir).expect("fake java dir");
        let java_path = bin_dir.join("java");
        fs::write(&java_path, probe_output_script).expect("fake java");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = fs::metadata(&java_path)
                .expect("fake java metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&java_path, permissions).expect("fake java permissions");
        }
        java_path
    }

    fn write_fabric_version(library_dir: &Path, target: RepresentativeTarget) -> String {
        let version_id = format!(
            "fabric-loader-{}-{}",
            target.loader_version, target.minecraft_version
        );
        let base_version_dir = library_dir.join("versions").join(target.minecraft_version);
        let fabric_version_dir = library_dir.join("versions").join(&version_id);
        fs::create_dir_all(&base_version_dir).expect("base version dir");
        fs::create_dir_all(&fabric_version_dir).expect("fabric version dir");

        fs::write(
            base_version_dir.join(format!("{}.jar", target.minecraft_version)),
            b"client jar",
        )
        .expect("base client jar");
        fs::write(
            fabric_version_dir.join(format!("{version_id}.jar")),
            b"loader jar",
        )
        .expect("fabric version jar");

        write_library_jar(
            library_dir,
            &format!(
                "net/fabricmc/fabric-loader/{}/fabric-loader-{}.jar",
                target.loader_version, target.loader_version
            ),
        );
        write_library_jar(
            library_dir,
            &format!(
                "net/fabricmc/intermediary/{}/intermediary-{}.jar",
                target.minecraft_version, target.minecraft_version
            ),
        );
        write_library_jar(
            library_dir,
            &format!(
                "com/example/representative-{}/1.0.0/representative-{}-1.0.0.jar",
                target.family.to_ascii_lowercase(),
                target.family.to_ascii_lowercase()
            ),
        );

        write_version_json(
            &base_version_dir.join(format!("{}.json", target.minecraft_version)),
            serde_json::json!({
                "id": target.minecraft_version,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "javaVersion": {
                    "component": if target.java_major >= 21 {
                        "java-runtime-delta"
                    } else {
                        "java-runtime-gamma"
                    },
                    "majorVersion": target.java_major
                },
                "assetIndex": { "id": format!("{}-assets", target.minecraft_version) },
                "arguments": {
                    "jvm": [
                        "-Djava.library.path=${natives_directory}",
                        "-cp",
                        "${classpath}"
                    ],
                    "game": [
                        "--username",
                        "${auth_player_name}",
                        "--version",
                        "${version_name}",
                        "--gameDir",
                        "${game_directory}",
                        "--assetsDir",
                        "${assets_root}",
                        "--assetIndex",
                        "${asset_index_name}",
                        "--accessToken",
                        "${auth_access_token}",
                        "--uuid",
                        "${auth_uuid}",
                        "--userType",
                        "${user_type}",
                        "--versionType",
                        "${version_type}"
                    ]
                },
                "libraries": [{
                    "name": format!("com.example:representative-{}:1.0.0", target.family.to_ascii_lowercase()),
                    "downloads": {
                        "artifact": {
                            "path": format!("com/example/representative-{}/1.0.0/representative-{}-1.0.0.jar", target.family.to_ascii_lowercase(), target.family.to_ascii_lowercase()),
                            "url": "https://example.invalid/representative.jar"
                        }
                    }
                }]
            }),
        );

        write_version_json(
            &fabric_version_dir.join(format!("{version_id}.json")),
            serde_json::json!({
                "id": version_id,
                "inheritsFrom": target.minecraft_version,
                "type": "release",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "assetIndex": {},
                "arguments": {
                    "jvm": [],
                    "game": []
                },
                "libraries": [
                    {
                        "name": format!("net.fabricmc:fabric-loader:{}", target.loader_version),
                        "downloads": {
                            "artifact": {
                                "path": format!("net/fabricmc/fabric-loader/{}/fabric-loader-{}.jar", target.loader_version, target.loader_version),
                                "url": "https://example.invalid/fabric-loader.jar"
                            }
                        }
                    },
                    {
                        "name": format!("net.fabricmc:intermediary:{}", target.minecraft_version),
                        "downloads": {
                            "artifact": {
                                "path": format!("net/fabricmc/intermediary/{}/intermediary-{}.jar", target.minecraft_version, target.minecraft_version),
                                "url": "https://example.invalid/intermediary.jar"
                            }
                        }
                    }
                ]
            }),
        );

        version_id
    }

    fn write_library_jar(library_dir: &Path, relative_path: &str) {
        let path = library_dir.join("libraries").join(relative_path);
        fs::create_dir_all(path.parent().expect("library parent")).expect("library dir");
        fs::write(path, b"library jar").expect("library jar");
    }

    fn write_version_json(path: &Path, value: serde_json::Value) {
        fs::write(
            path,
            serde_json::to_vec_pretty(&value).expect("serialize version json"),
        )
        .expect("version json");
    }

    fn assert_arg_value(args: &[String], name: &str, expected: &str) {
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == name && pair[1] == expected),
            "expected {name} to be followed by {expected:?} in {args:?}"
        );
    }
}
