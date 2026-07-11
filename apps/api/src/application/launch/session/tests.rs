use super::*;
use crate::guardian::GuardianSeverity;
use crate::state::contracts::{CommandKind, OperationOutcome, OperationStatus, OwnershipClass};
use crate::state::failure_memory::FailureMemoryActionOutcome;
use crate::state::{
    AppStateInit, AuthLoginMinecraftProfile, InstallStore, NewAuthLoginMinecraftAccount,
    NewAuthLoginMsaToken, SessionStore,
};
use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
use axial_launcher::{
    GuardianDecision, LAUNCH_DISK_HEADROOM_MB, LAUNCH_MEMORY_HEADROOM_MB, LaunchReadinessReason,
    LaunchReadinessReasonId, LaunchReadinessSeverity, OverrideOrigin, SessionId,
};
use axial_performance::PerformanceManager;
use axum::Json;
use sha1::{Digest, Sha1};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

mod errors;
mod overrides;
mod prepare;
mod readiness;
mod resources;
mod warnings;

struct TestFixture {
    state: AppState,
    paths: AppPaths,
    root: PathBuf,
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let root = test_root(name);
        let paths = test_paths(&root);
        fs::create_dir_all(&paths.library_dir).expect("create library dir");
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: paths.library_dir.to_string_lossy().to_string(),
                    ..AppConfig::default()
                },
            )
            .expect("set library dir"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("load instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });

        Self { state, paths, root }
    }

    fn add_instance(&self, name: &str, version_id: &str) -> String {
        self.state
            .instances()
            .insert_for_test(name.to_string(), version_id.to_string())
            .expect("add instance")
            .id
    }

    fn write_ready_install(&self, version_id: &str) {
        self.write_ready_install_with_java(version_id, "java-runtime-delta", 21);
    }

    fn write_ready_install_with_java(&self, version_id: &str, component: &str, major_version: i32) {
        self.write_version_json(
            version_id,
            serde_json::json!({
                "id": version_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "javaVersion": { "component": component, "majorVersion": major_version },
                "libraries": []
            }),
        );
        let version_dir = self.paths.library_dir.join("versions").join(version_id);
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write client jar");
        self.write_ready_runtime(component);
    }

    fn write_child_version(&self, version_id: &str, parent_id: &str) {
        self.write_version_json(
            version_id,
            serde_json::json!({
                "id": version_id,
                "inheritsFrom": parent_id,
                "type": "release",
                "mainClass": "net.minecraft.client.main.Main",
                "assetIndex": {},
                "libraries": []
            }),
        );
    }

    fn write_ready_runtime(&self, component: &str) {
        let runtime_root = self.paths.library_dir.join("runtime").join(component);
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(java_path.parent().expect("runtime bin")).expect("runtime bin");
        fs::write(&java_path, b"java").expect("runtime java");
        make_executable(&java_path);
    }

    fn write_global_runtime_without_ready_marker(&self, component: &str) -> PathBuf {
        let runtime_root = self.paths.config_dir.join("runtimes").join(component);
        let java_path = managed_runtime_java_path(&runtime_root);
        fs::create_dir_all(java_path.parent().expect("global runtime java parent"))
            .expect("global runtime java parent");
        fs::write(&java_path, b"java").expect("global runtime java");
        make_executable(&java_path);
        write_runtime_manifest_proof(&runtime_root, &java_path);
        runtime_root
    }

    fn write_manual_java_override(&self) -> String {
        let bin_dir = self.root.join("manual-java").join("bin");
        fs::create_dir_all(&bin_dir).expect("manual java bin");
        let java_path = bin_dir.join("java");
        fs::write(
            &java_path,
            "#!/bin/sh\ncat >&2 <<'AXIAL_JAVA_PROBE'\nopenjdk version \"21.0.2\" 2024-01-16\njava.vendor = Eclipse Adoptium\njava.runtime.name = OpenJDK Runtime Environment\nAXIAL_JAVA_PROBE\n",
        )
        .expect("manual java");
        make_executable(&java_path);
        java_path.to_string_lossy().to_string()
    }

    fn update_instance(&self, id: &str, update: impl FnOnce(&mut Instance)) {
        let mut instance = self.state.instances().get(id).expect("instance");
        update(&mut instance);
        self.state
            .instances()
            .replace_for_test(instance)
            .expect("update instance");
    }

    fn set_guardian_mode(&self, mode: &str) {
        let mut config = self.state.config().current();
        config.guardian_mode = mode.to_string();
        self.state
            .config()
            .replace_for_test(config)
            .expect("set guardian mode");
    }

    fn set_launch_auth_mode(&self, mode: &str) {
        let mut config = self.state.config().current();
        config.launch_auth_mode = mode.to_string();
        self.state
            .config()
            .replace_for_test(config)
            .expect("set launch auth mode");
    }

    fn set_global_jvm_preset(&self, preset: &str) {
        let mut config = self.state.config().current();
        config.jvm_preset = preset.to_string();
        self.state
            .config()
            .replace_for_test(config)
            .expect("set global jvm preset");
    }

    fn set_global_java_override(&self, java_path: &str) {
        let mut config = self.state.config().current();
        config.java_path_override = java_path.to_string();
        self.state
            .config()
            .replace_for_test(config)
            .expect("set global java override");
    }

    fn write_version_json(&self, version_id: &str, value: serde_json::Value) {
        let version_dir = self.paths.library_dir.join("versions").join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec(&value).expect("version json"),
        )
        .expect("write version json");
    }

    async fn prepare(
        &self,
        instance_id: String,
        max_memory_mb: Option<i32>,
    ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
        self.prepare_with_memory(instance_id, max_memory_mb, None)
            .await
    }

    async fn prepare_with_memory(
        &self,
        instance_id: String,
        max_memory_mb: Option<i32>,
        min_memory_mb: Option<i32>,
    ) -> Result<PreparedLaunch, (StatusCode, Json<serde_json::Value>)> {
        if let Some(instance) = self.state.instances().get(&instance_id) {
            self.write_ready_install(&instance.version_id);
        }
        prepare_launch_session(
            &self.state,
            LaunchRequest {
                instance_id,
                username: None,
                max_memory_mb,
                min_memory_mb,
                client_started_at_ms: None,
            },
        )
        .await
    }

    async fn add_active_launch(&self, session_id: &str, max_memory_mb: u64) {
        self.state
            .sessions()
            .insert(LaunchSessionRecord {
                session_id: SessionId(session_id.to_string()),
                instance_id: format!("{session_id}-instance"),
                version_id: "1.21.1".to_string(),
                launched_at: Some(timestamp_utc()),
                benchmark: None,
                state: LaunchState::Queued,
                pid: None,
                process_started_at_ms: None,
                boot_completed_at_ms: None,
                boot_duration_ms: None,
                priority: None,
                exit_code: None,
                command: vec!["java".to_string(), format!("-Xmx{max_memory_mb}M")],
                java_path: None,
                natives_dir: None,
                failure: None,
                crash_evidence: None,
                healing: None,
                guardian: None,
                outcome: None,
                stages: Vec::new(),
            })
            .await
            .expect("insert session");
    }

    async fn add_active_install(&self, install_id: &str) {
        self.state.installs().insert(install_id.to_string()).await;
    }

    async fn add_active_minecraft_account(&self, owns_minecraft_java: bool) {
        self.state
            .auth_logins()
            .replace_with_msa_and_minecraft_account(
                NewAuthLoginMsaToken {
                    access_token: "msa-access-token".to_string(),
                    refresh_token: None,
                    id_token: None,
                    token_type: "Bearer".to_string(),
                    expires_in: 3600,
                    scope: Some("XboxLive.signin offline_access".to_string()),
                },
                NewAuthLoginMinecraftAccount {
                    access_token: "minecraft-access-token".to_string(),
                    token_type: Some("Bearer".to_string()),
                    expires_in: 86400,
                    profile: AuthLoginMinecraftProfile {
                        id: "4f9c7f7d0b1245d9a5c2f03a8c120001".to_string(),
                        name: "ProfileName".to_string(),
                        skins: Vec::new(),
                        capes: Vec::new(),
                    },
                    owns_minecraft_java,
                },
            )
            .await
            .expect("insert launch auth fixture");
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).expect("java metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("java executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn write_runtime_manifest_proof(runtime_root: &Path, java_path: &Path) {
    let bytes = fs::read(java_path).expect("read fake java");
    let relative_path = java_path
        .strip_prefix(runtime_root)
        .expect("java under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    let sha1 = format!("{:x}", hasher.finalize());
    let manifest = serde_json::json!({
        "files": {
            relative_path: {
                "type": "file",
                "downloads": {
                    "raw": {
                        "url": "https://example.invalid/java",
                        "sha1": sha1,
                        "size": bytes.len()
                    }
                }
            }
        }
    });
    fs::write(
        runtime_root.join(".axial-runtime-manifest.json"),
        serde_json::to_vec(&manifest).expect("manifest json"),
    )
    .expect("runtime manifest proof");
}

fn managed_runtime_java_path(runtime_root: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return runtime_root
            .join("jre.bundle")
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java");
    }

    runtime_root
        .join("bin")
        .join(if cfg!(target_os = "windows") {
            "javaw.exe"
        } else {
            "java"
        })
}

fn test_root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "axial-api-launch-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create test root");
    path
}

fn test_paths(root: &Path) -> AppPaths {
    let config_dir = root.join("config");
    AppPaths {
        config_file: config_dir.join("config.json"),
        instances_file: config_dir.join("instances.json"),
        instances_dir: root.join("instances"),
        music_dir: root.join("music"),
        library_dir: root.join("library"),
        config_dir,
    }
}

fn assert_readiness_reason(preflight: &LaunchPreflightResponse, expected: LaunchReadinessReasonId) {
    assert!(
        preflight
            .readiness
            .reasons
            .iter()
            .any(|reason| reason.id == expected),
        "missing readiness reason {expected:?}: {:?}",
        preflight.readiness.reasons
    );
}

fn assert_guardian_fact(preflight: &LaunchPreflightResponse, expected: &str) {
    let _ = guardian_fact(preflight, expected);
}

fn guardian_fact<'a>(preflight: &'a LaunchPreflightResponse, expected: &str) -> &'a GuardianFact {
    preflight
        .guardian_facts
        .iter()
        .find(|fact| fact.id.as_str() == expected)
        .unwrap_or_else(|| {
            panic!(
                "missing guardian fact {expected}: {:?}",
                preflight.guardian_facts
            )
        })
}

fn readiness_reason(
    preflight: &LaunchPreflightResponse,
    expected: LaunchReadinessReasonId,
) -> &LaunchReadinessReason {
    preflight
        .readiness
        .reasons
        .iter()
        .find(|reason| reason.id == expected)
        .unwrap_or_else(|| {
            panic!(
                "missing readiness reason {expected:?}: {:?}",
                preflight.readiness.reasons
            )
        })
}

fn assert_launch_error_is_token_safe(value: &serde_json::Value) {
    assert_no_sensitive_public_field_keys(value);
    let text = value.to_string();
    for material in [
        "new-msa-access-token",
        "new-msa-refresh-token",
        "old-msa-access-token",
        "old-msa-refresh-token",
        "minecraft-access-token",
        "xbl-token",
        "xsts-token",
        "provider-secret-payload",
    ] {
        assert!(
            !text.contains(material),
            "public launch JSON exposed sensitive material {material}"
        );
    }
}

fn assert_no_sensitive_public_field_keys(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                assert!(
                    !matches!(
                        key.as_str(),
                        "access_token" | "refresh_token" | "id_token" | "device_code"
                    ),
                    "public launch JSON exposed {key}"
                );
                assert_no_sensitive_public_field_keys(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                assert_no_sensitive_public_field_keys(value);
            }
        }
        _ => {}
    }
}

fn test_budget_with_memory(
    memory_evidence: LaunchMemoryEvidence,
    host_cpu_threads: Option<usize>,
    active_session_count: usize,
    active_install_count: usize,
    active_memory_allocation_mb: u64,
    requested_memory_mb: i32,
) -> LaunchProofResourceBudget {
    test_budget_with_memory_and_disk(
        memory_evidence,
        LaunchDiskEvidence::default(),
        LaunchCpuLoadEvidence::default(),
        host_cpu_threads,
        ActiveLaunchResourceUse {
            session_count: active_session_count,
            install_count: active_install_count,
            memory_allocation_mb: active_memory_allocation_mb,
        },
        requested_memory_mb,
    )
}

fn test_budget_with_memory_and_disk(
    memory_evidence: LaunchMemoryEvidence,
    disk_evidence: LaunchDiskEvidence,
    cpu_load_evidence: LaunchCpuLoadEvidence,
    host_cpu_threads: Option<usize>,
    active: ActiveLaunchResourceUse,
    requested_memory_mb: i32,
) -> LaunchProofResourceBudget {
    capture_resource_budget_snapshot(
        memory_evidence,
        disk_evidence,
        cpu_load_evidence,
        host_cpu_threads,
        active,
        requested_memory_mb,
    )
}

fn assert_has_memory_clamp_warning(guardian: &GuardianSummary) {
    for expected in [
        "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
    ] {
        assert!(
            guardian.guidance.iter().any(|detail| detail == expected),
            "missing clamp guidance: {expected}"
        );
        assert!(
            guardian.details.iter().any(|detail| detail == expected),
            "missing clamp detail: {expected}"
        );
    }
}

fn assert_no_memory_clamp_warning(guardian: &GuardianSummary) {
    for unexpected in [
        "Minimum memory was higher than maximum memory, so Axial clamped the launch minimum to match the maximum allocation.",
        "Lower the minimum memory setting or raise the maximum memory allocation if this was intentional.",
    ] {
        assert!(
            !guardian.guidance.iter().any(|detail| detail == unexpected),
            "unexpected clamp guidance: {unexpected}"
        );
        assert!(
            !guardian.details.iter().any(|detail| detail == unexpected),
            "unexpected clamp detail: {unexpected}"
        );
    }
}

fn assert_has_low_memory_allocation_warning(guardian: &GuardianSummary, _max_memory_mb: i32) {
    for expected in [
        "Launch memory allocation is very low for Minecraft.".to_string(),
        "Raise the maximum memory allocation if Minecraft crashes during startup, stalls while loading, or exits with out-of-memory errors.".to_string(),
    ] {
        assert!(
            guardian.guidance.iter().any(|detail| detail == &expected),
            "missing low-memory guidance: {expected}"
        );
        assert!(
            guardian.details.iter().any(|detail| detail == &expected),
            "missing low-memory detail: {expected}"
        );
    }
}
