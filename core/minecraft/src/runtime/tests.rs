use super::{
    ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
    ComponentManifestFile, JavaRuntimeInfo, JavaRuntimeLookupError, MachOArm64Compatibility,
    ManagedRuntimeCache, RosettaRuntimeDecision, RuntimeDownloadActual, RuntimeDownloadEvidence,
    RuntimeDownloadIntegrityError, RuntimeDownloadManifest, RuntimeEnsureEvent, RuntimeId,
    RuntimeInstallState, RuntimeManifest, RuntimeRecord, RuntimeSource,
    acquire_runtime_source_for_test, component_manifest_destination,
    component_manifest_proof_bytes, detect_distribution, detect_runtime_state,
    ensure_runtime_with_events, fetch_runtime_file, fetch_runtime_manifest_bytes_for_test,
    install_managed_runtime, install_runtime_manifest_file, install_runtime_manifest_files,
    java_executable, java_executable_for_os, managed_runtime_contents_verified_without_probe,
    parse_mach_o_arm64_compatibility, plan_runtime_manifest_files, remove_runtime_install_path,
    remove_runtime_install_path_async, rosetta_requirement_for_managed_runtime,
    runtime_download_client, runtime_file_download_concurrency_for, runtime_install_lock_file_path,
    runtime_os_arch_for, runtime_record_matches_source_for_test,
    runtime_source_url_is_secure_for_test, runtime_windows_verbatim_path_string,
    select_runtime_manifest, validate_ephemeral_processor_manifest_for_test,
    verify_runtime_download,
};
use crate::JavaVersion;
use serde::Deserialize;
use sha1::{Digest as _, Sha1};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn expected(size: Option<u64>, sha1: Option<&str>) -> RuntimeDownloadEvidence {
    RuntimeDownloadEvidence {
        size,
        sha1: sha1.map(str::to_string),
    }
}

fn actual(size: u64, sha1: &str) -> RuntimeDownloadActual {
    RuntimeDownloadActual {
        size,
        sha1: sha1.to_string(),
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn manifest_file(kind: &str) -> ComponentManifestFile {
    ComponentManifestFile {
        kind: kind.to_string(),
        executable: false,
        downloads: None,
        target: None,
    }
}

fn downloadable_manifest_file(url: &str, size: u64, sha1: &str) -> ComponentManifestFile {
    ComponentManifestFile {
        kind: "file".to_string(),
        executable: false,
        downloads: Some(ComponentManifestDownloads {
            raw: Some(ComponentManifestDownload {
                url: url.to_string(),
                sha1: Some(sha1.to_string()),
                size: Some(size),
            }),
            lzma: None,
        }),
        target: None,
    }
}

fn downloadable_lzma_manifest_file(
    raw_url: &str,
    raw_size: u64,
    raw_sha1: &str,
    lzma_url: &str,
    lzma_size: u64,
    lzma_sha1: &str,
) -> ComponentManifestFile {
    ComponentManifestFile {
        kind: "file".to_string(),
        executable: false,
        downloads: Some(ComponentManifestDownloads {
            raw: Some(ComponentManifestDownload {
                url: raw_url.to_string(),
                sha1: Some(raw_sha1.to_string()),
                size: Some(raw_size),
            }),
            lzma: Some(ComponentManifestDownload {
                url: lzma_url.to_string(),
                sha1: Some(lzma_sha1.to_string()),
                size: Some(lzma_size),
            }),
        }),
        target: None,
    }
}

fn manifest_link(target: &str) -> ComponentManifestFile {
    ComponentManifestFile {
        kind: "link".to_string(),
        executable: false,
        downloads: None,
        target: Some(target.to_string()),
    }
}

fn planned_paths(entries: &[(String, ComponentManifestFile)]) -> Vec<&str> {
    entries
        .iter()
        .map(|(relative_path, _)| relative_path.as_str())
        .collect()
}

fn unsafe_manifest_path_message(result: Result<PathBuf, JavaRuntimeLookupError>) -> String {
    match result {
        Err(JavaRuntimeLookupError::Download(message)) => message,
        other => panic!("expected unsafe manifest path error, got {other:?}"),
    }
}

fn assert_runtime_distribution(text: &str, expected: &str) {
    assert_eq!(detect_distribution(text), expected);
}

#[test]
fn detect_runtime_distribution_favors_graalvm_identity() {
    assert_runtime_distribution(
        r#"
                java.vendor = Oracle Corporation
                java.vm.name = GraalVM 64-Bit Server VM
            "#,
        "graalvm",
    );
    assert_runtime_distribution(
        r#"
                java.vendor = OpenJDK
                java.runtime.name = GraalVM Runtime Environment
            "#,
        "graalvm",
    );
}

#[test]
fn detect_runtime_distribution_classifies_openj9_identity() {
    for text in [
        "java.vm.name = Eclipse OpenJ9 VM",
        "java.runtime.name = IBM Semeru Runtime Open Edition",
        "java.vm.vendor = IBM Corporation",
    ] {
        assert_runtime_distribution(text, "openj9");
    }
}

#[test]
fn detect_runtime_distribution_classifies_temurin_identity() {
    for text in [
        "java.runtime.name = OpenJDK Runtime Environment Temurin-21.0.2+13",
        "java.vendor = Eclipse Adoptium",
        "java.vm.vendor = Eclipse Foundation",
    ] {
        assert_runtime_distribution(text, "temurin");
    }
}

#[test]
fn detect_runtime_distribution_classifies_oracle_identity() {
    assert_runtime_distribution(
        r#"
                java.vendor   =   Oracle Corporation
                java.vm.name = Java HotSpot(TM) 64-Bit Server VM
            "#,
        "oracle",
    );
}

#[test]
fn detect_runtime_distribution_classifies_generic_openjdk_identity() {
    assert_runtime_distribution(
        r#"
                java.vendor = Debian
                java.vm.name = OpenJDK 64-Bit Server VM
                java.runtime.version = 21.0.5+11-Debian-1
            "#,
        "openjdk",
    );
}

#[test]
fn detect_runtime_distribution_classifies_missing_identity_as_unknown() {
    assert_runtime_distribution(
        r#"
                java.home = /opt/java
                sun.arch.data.model = 64
            "#,
        "unknown",
    );
}

#[test]
fn component_manifest_destination_accepts_safe_nested_path() {
    let temp_dir = Path::new("runtime-temp");
    let destination = component_manifest_destination(temp_dir, "bin/java").unwrap();

    assert_eq!(destination, temp_dir.join("bin").join("java"));
}

#[test]
fn component_manifest_destination_rejects_traversal() {
    let temp_dir = Path::new("runtime-temp");
    let message =
        unsafe_manifest_path_message(component_manifest_destination(temp_dir, "bin/../java"));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains("bin/../java"));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn component_manifest_destination_rejects_absolute_path() {
    let temp_dir = Path::new("runtime-temp");
    let absolute_path = if cfg!(windows) {
        r"\Windows\System32"
    } else {
        "/etc/passwd"
    };
    let message =
        unsafe_manifest_path_message(component_manifest_destination(temp_dir, absolute_path));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains(absolute_path));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn component_manifest_destination_rejects_drive_like_path_with_slashes() {
    let temp_dir = Path::new("runtime-temp");
    let message = unsafe_manifest_path_message(component_manifest_destination(
        temp_dir,
        "C:/Windows/System32",
    ));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains("C:/Windows/System32"));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn component_manifest_destination_rejects_drive_like_path_with_backslashes() {
    let temp_dir = Path::new("runtime-temp");
    let message = unsafe_manifest_path_message(component_manifest_destination(
        temp_dir,
        r"C:\Windows\System32",
    ));

    assert!(message.contains("unsafe runtime manifest path"));
    assert!(message.contains(r"C:\Windows\System32"));
    assert!(!message.contains("runtime-temp"));
}

#[test]
fn runtime_windows_verbatim_path_transform_handles_deep_runtime_paths() {
    assert_eq!(
        runtime_windows_verbatim_path_string(
            r"C:/Users/Alice/AppData/Roaming/axial/runtimes/java-runtime-delta/bin/javaw.exe"
        ),
        r"\\?\C:\Users\Alice\AppData\Roaming\axial\runtimes\java-runtime-delta\bin\javaw.exe"
    );
    assert_eq!(
        runtime_windows_verbatim_path_string(
            r"\\server\share\axial\runtimes\java-runtime-delta\lib\jvm.cfg"
        ),
        r"\\?\UNC\server\share\axial\runtimes\java-runtime-delta\lib\jvm.cfg"
    );
    assert_eq!(
        runtime_windows_verbatim_path_string(r"\\?\C:\already\verbatim\javaw.exe"),
        r"\\?\C:\already\verbatim\javaw.exe"
    );
}

#[test]
fn runtime_file_download_concurrency_is_adaptive_and_bounded() {
    assert_eq!(runtime_file_download_concurrency_for(0), 8);
    assert_eq!(runtime_file_download_concurrency_for(1), 8);
    assert_eq!(runtime_file_download_concurrency_for(2), 8);
    assert_eq!(runtime_file_download_concurrency_for(3), 12);
    assert_eq!(runtime_file_download_concurrency_for(8), 32);
    assert_eq!(runtime_file_download_concurrency_for(64), 32);
}

#[test]
fn runtime_manifest_install_plan_sorts_directories_before_files() {
    let mut files = HashMap::new();
    files.insert("lib/server/libjvm.so".to_string(), manifest_file("file"));
    files.insert("bin/java".to_string(), manifest_file("file"));
    files.insert("lib/server".to_string(), manifest_file("directory"));
    files.insert("bin".to_string(), manifest_file("directory"));
    files.insert(
        "legal/module/LICENSE".to_string(),
        manifest_link("../base/LICENSE"),
    );
    files.insert("ignored-entry".to_string(), manifest_file("unknown"));

    let plan = plan_runtime_manifest_files(files);

    assert_eq!(
        planned_paths(&plan.directory_entries),
        vec!["bin", "lib/server"]
    );
    assert_eq!(
        planned_paths(&plan.file_entries),
        vec!["bin/java", "lib/server/libjvm.so"]
    );
    assert_eq!(
        planned_paths(&plan.link_entries),
        vec!["legal/module/LICENSE"]
    );
    assert_eq!(planned_paths(&plan.other_entries), vec!["ignored-entry"]);
}

#[tokio::test]
async fn runtime_manifest_install_reports_file_progress() {
    let root = unique_temp_root("axial-runtime-progress-test");
    let java_bytes = b"java".to_vec();
    let cfg_bytes = b"cfg".to_vec();
    let java_sha1 = sha1_hex(&java_bytes);
    let cfg_sha1 = sha1_hex(&cfg_bytes);
    let java_url = serve_runtime_download(java_bytes.clone()).await;
    let cfg_url = serve_runtime_response(
        200,
        cfg_bytes.clone(),
        Some(cfg_bytes.len() as u64),
        "/jvm.cfg",
    )
    .await;
    let mut files = HashMap::new();
    files.insert("bin".to_string(), manifest_file("directory"));
    files.insert(
        "bin/java".to_string(),
        downloadable_manifest_file(&java_url, java_bytes.len() as u64, &java_sha1),
    );
    files.insert(
        "lib/jvm.cfg".to_string(),
        downloadable_manifest_file(&cfg_url, cfg_bytes.len() as u64, &cfg_sha1),
    );
    let mut events = Vec::new();

    install_runtime_manifest_files("java-runtime-delta", &root, files, &mut |event| {
        events.push(event);
    })
    .await
    .expect("runtime manifest files");

    assert_eq!(
        events.first(),
        Some(&RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: "java-runtime-delta".to_string(),
            current: 0,
            total: 2,
            bytes_done: 0,
            bytes_total: (b"java".len() + b"cfg".len()) as u64,
        })
    );
    assert_eq!(events.len(), 3);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEnsureEvent::InstallingManagedRuntimeFiles { current, total, .. } => {
                    Some((*current, *total))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![(0, 2), (1, 2), (2, 2)]
    );
    assert!(events.iter().all(|event| matches!(
        event,
        RuntimeEnsureEvent::InstallingManagedRuntimeFiles { .. }
    )));
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_manifest_install_creates_link_entries_and_counts_them() {
    let root = unique_temp_root("axial-runtime-link-install-test");
    let license_bytes = b"license".to_vec();
    let license_sha1 = sha1_hex(&license_bytes);
    let license_url = serve_runtime_download(license_bytes.clone()).await;
    let mut files = HashMap::new();
    files.insert(
        "legal/java.base/LICENSE".to_string(),
        downloadable_manifest_file(&license_url, license_bytes.len() as u64, &license_sha1),
    );
    files.insert(
        "legal/java.compiler/LICENSE".to_string(),
        manifest_link("../java.base/LICENSE"),
    );
    let mut events = Vec::new();

    install_runtime_manifest_files("java-runtime-delta", &root, files, &mut |event| {
        events.push(event);
    })
    .await
    .expect("runtime manifest with link");

    assert_eq!(
        fs::read_link(root.join("legal/java.compiler/LICENSE")).expect("runtime link"),
        PathBuf::from("../java.base/LICENSE")
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
                    current,
                    total,
                    bytes_done,
                    bytes_total,
                    ..
                } => Some((*current, *total, *bytes_done, *bytes_total)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            (0, 2, 0, license_bytes.len() as u64),
            (1, 2, license_bytes.len() as u64, license_bytes.len() as u64),
            (2, 2, license_bytes.len() as u64, license_bytes.len() as u64)
        ]
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn runtime_manifest_link_rejects_target_escape() {
    let root = unique_temp_root("axial-runtime-link-escape-test");
    let result = install_runtime_manifest_file(
        runtime_download_client().clone(),
        &root,
        "bin/java-link",
        manifest_link("../../outside"),
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::Download(message))
            if message.contains("unsafe runtime manifest link target")
    ));
    assert!(!root.join("bin").join("java-link").exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(not(unix))]
#[tokio::test]
async fn runtime_manifest_link_fails_explicitly_on_non_unix() {
    let root = unique_temp_root("axial-runtime-link-non-unix-test");
    let result = install_runtime_manifest_file(
        runtime_download_client().clone(),
        &root,
        "bin/java-link",
        manifest_link("java"),
    )
    .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::Download(message))
            if message.contains("unsupported on this platform")
    ));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_json_fetch_reads_async_http_body() {
    #[derive(Debug, Deserialize)]
    struct SampleRuntimeManifest {
        ok: bool,
    }

    let url = serve_runtime_json(200, r#"{"ok":true}"#.as_bytes().to_vec(), None).await;

    let bytes = fetch_runtime_manifest_bytes_for_test(&url)
        .await
        .expect("runtime manifest bytes");
    let manifest =
        serde_json::from_slice::<SampleRuntimeManifest>(&bytes).expect("runtime manifest json");

    assert!(manifest.ok);
}

#[tokio::test]
async fn runtime_manifest_json_fetch_rejects_http_errors() {
    let url = serve_runtime_json(503, b"unavailable".to_vec(), None).await;

    let error = fetch_runtime_manifest_bytes_for_test(&url)
        .await
        .expect_err("HTTP error should fail");

    assert!(error.to_string().contains("HTTP 503"), "{error}");
}

#[tokio::test]
async fn runtime_manifest_json_fetch_rejects_oversized_content_length() {
    let url = serve_runtime_json(
        200,
        b"ignored".to_vec(),
        Some(super::MAX_RUNTIME_MANIFEST_BYTES + 1),
    )
    .await;

    let error = fetch_runtime_manifest_bytes_for_test(&url)
        .await
        .expect_err("oversized manifest should fail");

    assert_eq!(
        error.to_string(),
        "failed to install java runtime: runtime manifest response too large"
    );
}

#[test]
fn production_runtime_source_policy_accepts_only_https_urls() {
    assert!(runtime_source_url_is_secure_for_test(
        "https://launchermeta.mojang.com/runtime.json"
    ));
    assert!(!runtime_source_url_is_secure_for_test(
        "http://launchermeta.mojang.com/runtime.json"
    ));
    assert!(!runtime_source_url_is_secure_for_test(
        "file:///tmp/runtime.json"
    ));
    assert!(!runtime_source_url_is_secure_for_test("not a URL"));
}

#[tokio::test]
async fn runtime_source_receipt_preserves_verified_authored_bytes() {
    let bytes = br#"{"files":{}}"#.to_vec();
    let url = serve_runtime_json(200, bytes.clone(), None).await;
    let expected_sha1 = sha1_hex(&bytes);
    let receipt = acquire_runtime_source_for_test(
        RuntimeId::from("java-runtime-delta"),
        RuntimeDownloadManifest {
            url,
            sha1: expected_sha1.clone(),
            size: bytes.len() as u64,
        },
    )
    .await
    .expect("authenticated runtime source");

    assert_eq!(receipt.component().as_str(), "java-runtime-delta");
    assert_eq!(receipt.bytes(), bytes);
    assert_eq!(receipt.expected_sha1(), expected_sha1);
    assert_eq!(receipt.expected_size(), bytes.len() as u64);
}

#[tokio::test]
async fn ready_managed_runtime_matches_only_the_exact_receipt_proof() {
    let root = unique_temp_root("axial-runtime-exact-receipt-proof");
    fs::create_dir_all(&root).expect("runtime root");
    let manifest = ComponentManifest {
        files: HashMap::new(),
    };
    let bytes = serde_json::to_vec(&manifest).expect("component manifest");
    let receipt = acquire_runtime_source_for_test(
        RuntimeId::from("java-runtime-delta"),
        RuntimeDownloadManifest {
            url: serve_runtime_json(200, bytes.clone(), None).await,
            sha1: sha1_hex(&bytes),
            size: bytes.len() as u64,
        },
    )
    .await
    .expect("runtime source receipt");
    let proof = component_manifest_proof_bytes(&manifest).expect("canonical proof");
    fs::write(root.join(super::COMPONENT_MANIFEST_PROOF_FILE), &proof)
        .expect("persist exact proof");
    let runtime = RuntimeRecord {
        id: RuntimeId::from("java-runtime-delta"),
        java_path: root.join("bin/java").to_string_lossy().into_owned(),
        info: JavaRuntimeInfo {
            id: "java-runtime-delta".to_string(),
            major: 21,
            update: 0,
            distribution: "test".to_string(),
            path: root.join("bin/java").to_string_lossy().into_owned(),
        },
        source: RuntimeSource::Managed,
        install_state: RuntimeInstallState::Ready,
        root_dir: root.to_string_lossy().into_owned(),
    };

    assert!(runtime_record_matches_source_for_test(&runtime, &receipt).await);
    fs::write(
        root.join(super::COMPONENT_MANIFEST_PROOF_FILE),
        b"different authenticated generation",
    )
    .expect("replace proof");
    assert!(!runtime_record_matches_source_for_test(&runtime, &receipt).await);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_source_receipt_rejects_size_mismatch_before_parse() {
    let bytes = b"not json".to_vec();
    let url = serve_runtime_json(200, bytes.clone(), None).await;
    let error = acquire_runtime_source_for_test(
        RuntimeId::from("java-runtime-delta"),
        RuntimeDownloadManifest {
            url,
            sha1: sha1_hex(&bytes),
            size: bytes.len() as u64 + 1,
        },
    )
    .await
    .expect_err("size mismatch must reject the source");

    assert_eq!(
        error.to_string(),
        "failed to install java runtime: runtime component manifest size mismatch"
    );
}

#[tokio::test]
async fn runtime_source_receipt_rejects_checksum_mismatch_before_parse() {
    let bytes = b"not json".to_vec();
    let url = serve_runtime_json(200, bytes.clone(), None).await;
    let error = acquire_runtime_source_for_test(
        RuntimeId::from("java-runtime-delta"),
        RuntimeDownloadManifest {
            url,
            sha1: "0000000000000000000000000000000000000000".to_string(),
            size: bytes.len() as u64,
        },
    )
    .await
    .expect_err("checksum mismatch must reject the source");

    assert_eq!(
        error.to_string(),
        "failed to install java runtime: runtime component manifest checksum mismatch"
    );
}

#[tokio::test]
async fn runtime_source_receipt_parses_only_after_integrity_validation() {
    let bytes = b"not json".to_vec();
    let url = serve_runtime_json(200, bytes.clone(), None).await;
    let error = acquire_runtime_source_for_test(
        RuntimeId::from("java-runtime-delta"),
        RuntimeDownloadManifest {
            url,
            sha1: sha1_hex(&bytes),
            size: bytes.len() as u64,
        },
    )
    .await
    .expect_err("authenticated malformed JSON must fail parsing");

    assert!(error.to_string().contains("expected ident"), "{error}");
}

#[test]
fn runtime_catalog_requires_component_manifest_integrity_proof() {
    let missing_sha1 = serde_json::from_value::<RuntimeManifest>(serde_json::json!({
        "linux": {
            "java-runtime-delta": [{
                "manifest": {
                    "url": "https://example.invalid/runtime.json",
                    "size": 1
                }
            }]
        }
    }));
    let missing_size = serde_json::from_value::<RuntimeManifest>(serde_json::json!({
        "linux": {
            "java-runtime-delta": [{
                "manifest": {
                    "url": "https://example.invalid/runtime.json",
                    "sha1": "0000000000000000000000000000000000000000"
                }
            }]
        }
    }));

    assert!(missing_sha1.is_err());
    assert!(missing_size.is_err());
}

#[test]
fn managed_runtime_cache_clones_share_root_and_install_lock() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let cloned = cache.clone();

    assert_eq!(cache.root(), cloned.root());
    assert!(Arc::ptr_eq(
        &cache.install_lock("java-runtime-delta"),
        &cloned.install_lock("java-runtime-delta"),
    ));
}

#[test]
fn managed_runtime_caches_isolate_roots_locks_and_component_binding() {
    let first = ManagedRuntimeCache::isolated_for_test().expect("first runtime cache");
    let second = ManagedRuntimeCache::isolated_for_test().expect("second runtime cache");
    let first_root = first
        .component_root("java-runtime-delta")
        .expect("first component root");

    assert_ne!(first.root(), second.root());
    assert!(!Arc::ptr_eq(
        &first.install_lock("java-runtime-delta"),
        &second.install_lock("java-runtime-delta"),
    ));
    assert_eq!(
        first.component_for_root(&first_root).as_deref(),
        Some("java-runtime-delta")
    );
    assert!(second.component_for_root(&first_root).is_none());
    assert!(
        second
            .component_for_path(&first_root.join("bin/java"))
            .is_none()
    );
}

#[test]
fn managed_runtime_cache_lives_until_the_final_clone_drops() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let root = cache.root().to_path_buf();
    let retained = cache.clone();

    drop(cache);
    assert!(root.is_dir());
    drop(retained);
    assert!(!root.exists());
}

#[test]
fn managed_runtime_cache_debug_is_redacted() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let debug = format!("{cache:?}");

    assert_eq!(debug, "ManagedRuntimeCache { .. }");
    assert!(!debug.contains(cache.root().to_string_lossy().as_ref()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_runtime_cache_root_is_stable_across_task_migration() {
    let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let expected = cache.root().to_path_buf();
    let observed = tokio::spawn(async move {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        cache.root().to_path_buf()
    })
    .await
    .expect("runtime cache task");

    assert_eq!(observed, expected);
}

#[test]
fn runtime_install_file_lock_path_is_component_sibling() {
    let install_root = Path::new("/runtime-cache").join("java-runtime-delta");

    assert_eq!(
        runtime_install_lock_file_path(&install_root),
        Path::new("/runtime-cache").join("java-runtime-delta.install.lock")
    );
}

#[test]
fn managed_runtime_requires_ready_marker_even_when_java_exists() {
    let root = unique_temp_root("axial-managed-runtime-ready-marker-test");
    write_runtime_executable_fixture(&root);

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Broken);

    fs::write(root.join(".axial-installing"), b"installing").expect("installing marker");
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Installing);

    fs::remove_file(root.join(".axial-installing")).expect("remove installing marker");
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Broken);
    write_runtime_manifest_proof_for_java(&root);
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn structural_runtime_discovery_does_not_parse_empty_manifest_proof() {
    let root = unique_temp_root("axial-managed-runtime-empty-proof-test");
    write_runtime_executable_fixture(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        br#"{"files":{}}"#,
    )
    .expect("empty runtime manifest proof");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(!managed_runtime_contents_verified_without_probe(&root));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn structural_runtime_discovery_does_not_parse_missing_raw_download_proof() {
    let root = unique_temp_root("axial-managed-runtime-missing-raw-proof-test");
    write_runtime_executable_fixture(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        br#"{"files":{"bin/java":{"type":"file","downloads":{}}}}"#,
    )
    .expect("runtime manifest proof without raw download");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(!managed_runtime_contents_verified_without_probe(&root));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_full_runtime_verifier_detects_same_root_content_drift() {
    let root = unique_temp_root("axial-managed-runtime-manifest-drift-test");
    write_runtime_executable_fixture(&root);
    write_runtime_manifest_proof_for_java(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(managed_runtime_contents_verified_without_probe(&root));

    fs::write(java_executable(&root), b"changed java").expect("modify java");
    make_executable(&java_executable(&root));

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(!managed_runtime_contents_verified_without_probe(&root));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn explicit_full_runtime_verifier_detects_manifest_link_drift() {
    let root = unique_temp_root("axial-managed-runtime-link-proof-test");
    write_runtime_executable_fixture(&root);
    let link = java_executable(&root).with_file_name("java-link");
    std::os::unix::fs::symlink("java", &link).expect("runtime symlink");
    write_runtime_manifest_proof_for_java_and_link(&root);
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(managed_runtime_contents_verified_without_probe(&root));

    fs::remove_file(link).expect("remove runtime symlink");
    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(!managed_runtime_contents_verified_without_probe(&root));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_install_cleanup_removes_stale_directory_destination() {
    let root = unique_temp_root("axial-runtime-cleanup-dir-test");
    fs::create_dir_all(root.join("bin")).expect("create stale runtime dir");
    fs::write(root.join("bin").join("java"), b"stale").expect("write stale java");

    remove_runtime_install_path(&root).expect("remove stale runtime dir");

    assert!(!root.exists());
}

#[test]
fn runtime_install_cleanup_removes_stale_file_destination() {
    let root = unique_temp_root("axial-runtime-cleanup-file-test");
    fs::write(&root, b"blocking file").expect("write stale runtime file");

    remove_runtime_install_path(&root).expect("remove stale runtime file");

    assert!(!root.exists());
}

#[test]
fn runtime_install_cleanup_accepts_missing_destination() {
    let root = unique_temp_root("axial-runtime-cleanup-missing-test");

    remove_runtime_install_path(&root).expect("missing runtime path is clean");

    assert!(!root.exists());
}

#[tokio::test]
async fn async_runtime_install_cleanup_removes_stale_directory_destination() {
    let root = unique_temp_root("axial-runtime-async-cleanup-dir-test");
    fs::create_dir_all(root.join("bin")).expect("create stale runtime dir");
    fs::write(root.join("bin").join("java"), b"stale").expect("write stale java");

    remove_runtime_install_path_async(&root)
        .await
        .expect("remove stale runtime dir");

    assert!(!root.exists());
}

#[tokio::test]
async fn async_runtime_install_cleanup_removes_stale_file_destination() {
    let root = unique_temp_root("axial-runtime-async-cleanup-file-test");
    fs::write(&root, b"blocking file").expect("write stale runtime file");

    remove_runtime_install_path_async(&root)
        .await
        .expect("remove stale runtime file");

    assert!(!root.exists());
}

#[tokio::test]
async fn async_runtime_install_cleanup_accepts_missing_destination() {
    let root = unique_temp_root("axial-runtime-async-cleanup-missing-test");

    remove_runtime_install_path_async(&root)
        .await
        .expect("missing runtime path is clean");

    assert!(!root.exists());
}

#[test]
fn runtime_os_arch_uses_mojang_manifest_platform_keys() {
    assert_eq!(runtime_os_arch_for("linux", "x86_64"), "linux");
    assert_eq!(runtime_os_arch_for("linux", "x86"), "linux-i386");
    assert_eq!(runtime_os_arch_for("linux", "aarch64"), "linux");
    assert_eq!(runtime_os_arch_for("macos", "x86_64"), "mac-os");
    assert_eq!(runtime_os_arch_for("macos", "aarch64"), "mac-os-arm64");
    assert_eq!(runtime_os_arch_for("windows", "x86_64"), "windows-x64");
    assert_eq!(runtime_os_arch_for("windows", "x86"), "windows-x86");
    assert_eq!(runtime_os_arch_for("windows", "aarch64"), "windows-arm64");
}

#[test]
fn runtime_manifest_selection_falls_back_from_macos_arm64_to_macos() {
    let manifest = runtime_manifest_fixture(serde_json::json!({
        "mac-os-arm64": {
            "jre-legacy": []
        },
        "mac-os": {
            "jre-legacy": [
                { "manifest": {
                    "url": "https://example.invalid/mac-os/jre-legacy.json",
                    "sha1": "0000000000000000000000000000000000000000",
                    "size": 1
                } }
            ]
        }
    }));

    let descriptor =
        select_runtime_manifest(&manifest, &RuntimeId::from("jre-legacy"), "mac-os-arm64")
            .expect("fallback runtime manifest descriptor");

    assert_eq!(
        descriptor.url,
        "https://example.invalid/mac-os/jre-legacy.json"
    );
}

#[test]
fn runtime_manifest_selection_uses_native_entries_before_fallbacks() {
    let manifest = runtime_manifest_fixture(serde_json::json!({
        "mac-os-arm64": {
            "jre-legacy": [
                { "manifest": {
                    "url": "https://example.invalid/mac-os-arm64/jre-legacy.json",
                    "sha1": "1111111111111111111111111111111111111111",
                    "size": 2
                } }
            ]
        },
        "mac-os": {
            "jre-legacy": [
                { "manifest": {
                    "url": "https://example.invalid/mac-os/jre-legacy.json",
                    "sha1": "2222222222222222222222222222222222222222",
                    "size": 3
                } }
            ]
        }
    }));

    let descriptor =
        select_runtime_manifest(&manifest, &RuntimeId::from("jre-legacy"), "mac-os-arm64")
            .expect("native runtime manifest descriptor");

    assert_eq!(
        descriptor.url,
        "https://example.invalid/mac-os-arm64/jre-legacy.json"
    );
    assert_eq!(descriptor.size, 2);
}

#[test]
fn runtime_manifest_selection_reports_unsupported_platform_after_empty_fallbacks() {
    let manifest = runtime_manifest_fixture(serde_json::json!({
        "mac-os-arm64": {
            "jre-legacy": []
        },
        "mac-os": {
            "jre-legacy": []
        }
    }));

    let error = select_runtime_manifest(&manifest, &RuntimeId::from("jre-legacy"), "mac-os-arm64")
        .expect_err("empty native and fallback runtimes should fail");

    assert!(matches!(
        error,
        JavaRuntimeLookupError::UnsupportedPlatform {
            component,
            platform
        } if component == "jre-legacy" && platform == "mac-os-arm64"
    ));
}

#[test]
fn mach_o_sniff_detects_thin_arm64_binary_as_compatible() {
    let mut bytes = Vec::new();
    bytes.extend(0xfeed_facfu32.to_le_bytes());
    bytes.extend(0x0100_000cu32.to_le_bytes());
    bytes.extend([0u8; 24]);

    assert_eq!(
        parse_mach_o_arm64_compatibility(&bytes),
        MachOArm64Compatibility::HasArm64Slice
    );
}

#[test]
fn mach_o_sniff_detects_thin_x86_64_binary_as_lacking_arm64() {
    let mut bytes = Vec::new();
    bytes.extend(0xfeed_facfu32.to_le_bytes());
    bytes.extend(0x0100_0007u32.to_le_bytes());
    bytes.extend([0u8; 24]);

    assert_eq!(
        parse_mach_o_arm64_compatibility(&bytes),
        MachOArm64Compatibility::LacksArm64Slice
    );
}

#[test]
fn mach_o_sniff_detects_fat_binary_with_arm64_slice_as_compatible() {
    let mut bytes = Vec::new();
    bytes.extend(0xcafe_babeu32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(fat_arch32_be(0x0100_0007));
    bytes.extend(fat_arch32_be(0x0100_000c));

    assert_eq!(
        parse_mach_o_arm64_compatibility(&bytes),
        MachOArm64Compatibility::HasArm64Slice
    );
}

#[test]
fn mach_o_sniff_detects_fat_binary_without_arm64_slice_as_lacking_arm64() {
    let mut bytes = Vec::new();
    bytes.extend(0xcafe_babeu32.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(fat_arch32_be(0x0100_0007));

    assert_eq!(
        parse_mach_o_arm64_compatibility(&bytes),
        MachOArm64Compatibility::LacksArm64Slice
    );
}

#[test]
fn mach_o_sniff_handles_fat64_binary_with_arm64_slice() {
    let mut bytes = Vec::new();
    bytes.extend(0xcafe_babfu32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(fat_arch64_be(0x0100_0007));
    bytes.extend(fat_arch64_be(0x0100_000c));

    assert_eq!(
        parse_mach_o_arm64_compatibility(&bytes),
        MachOArm64Compatibility::HasArm64Slice
    );
}

#[test]
fn mach_o_sniff_treats_garbage_as_conservatively_compatible() {
    assert_eq!(
        parse_mach_o_arm64_compatibility(b"not a mach-o"),
        MachOArm64Compatibility::UnknownCompatible
    );
}

#[test]
fn rosetta_requirement_policy_only_blocks_arm64_macos_without_rosetta_for_non_arm64_binary() {
    assert_eq!(
        rosetta_requirement_for_managed_runtime(
            "macos",
            "aarch64",
            false,
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        RosettaRuntimeDecision::RosettaRequired
    );
    for (host_os, host_arch, rosetta_present, binary) in [
        (
            "linux",
            "aarch64",
            false,
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        (
            "macos",
            "x86_64",
            false,
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        (
            "macos",
            "aarch64",
            true,
            MachOArm64Compatibility::LacksArm64Slice,
        ),
        (
            "macos",
            "aarch64",
            false,
            MachOArm64Compatibility::HasArm64Slice,
        ),
        (
            "macos",
            "aarch64",
            false,
            MachOArm64Compatibility::UnknownCompatible,
        ),
    ] {
        assert_eq!(
            rosetta_requirement_for_managed_runtime(host_os, host_arch, rosetta_present, binary,),
            RosettaRuntimeDecision::Compatible
        );
    }
}

fn fat_arch32_be(cputype: u32) -> [u8; 20] {
    let mut arch = [0u8; 20];
    arch[0..4].copy_from_slice(&cputype.to_be_bytes());
    arch
}

fn fat_arch64_be(cputype: u32) -> [u8; 32] {
    let mut arch = [0u8; 32];
    arch[0..4].copy_from_slice(&cputype.to_be_bytes());
    arch
}

#[tokio::test]
async fn fallback_selected_runtime_install_is_ready_with_manifest_proof() {
    let root = unique_temp_root("axial-runtime-fallback-install-test");
    let component = RuntimeId::from("jre-legacy");
    let java_bytes = b"fallback java".to_vec();
    let cfg_bytes = b"fallback cfg".to_vec();
    let java_url = serve_runtime_download(java_bytes.clone()).await;
    let cfg_url = serve_runtime_download(cfg_bytes.clone()).await;
    let mut files = HashMap::new();
    let java_relative_path = java_executable(&root)
        .strip_prefix(&root)
        .expect("java path under runtime root")
        .to_string_lossy()
        .replace('\\', "/");
    let mut java_file =
        downloadable_manifest_file(&java_url, java_bytes.len() as u64, &sha1_hex(&java_bytes));
    java_file.executable = true;
    files.insert(java_relative_path, java_file);
    files.insert(
        "lib/jvm.cfg".to_string(),
        downloadable_manifest_file(&cfg_url, cfg_bytes.len() as u64, &sha1_hex(&cfg_bytes)),
    );
    let component_manifest = ComponentManifest { files };
    let component_manifest_bytes =
        serde_json::to_vec(&component_manifest).expect("component manifest json");
    let component_manifest_url =
        serve_runtime_json(200, component_manifest_bytes.clone(), None).await;
    let manifest = runtime_manifest_fixture(serde_json::json!({
        "mac-os-arm64": {
            "jre-legacy": []
        },
        "mac-os": {
            "jre-legacy": [
                { "manifest": {
                    "url": component_manifest_url,
                    "sha1": sha1_hex(&component_manifest_bytes),
                    "size": component_manifest_bytes.len()
                } }
            ]
        }
    }));
    let descriptor = select_runtime_manifest(&manifest, &component, "mac-os-arm64")
        .expect("fallback manifest descriptor")
        .clone();
    let receipt = acquire_runtime_source_for_test(component.clone(), descriptor)
        .await
        .expect("verified runtime source receipt");
    let mut events = Vec::new();

    install_managed_runtime(&component, &root, &receipt, &mut |event| events.push(event))
        .await
        .expect("fallback runtime install");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Ready);
    assert!(root.join(".axial-runtime-manifest.json").is_file());
    assert_eq!(
        events.last(),
        Some(&RuntimeEnsureEvent::ManagedRuntimeReady {
            component: "jre-legacy".to_string()
        })
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn java_executable_uses_platform_runtime_layouts() {
    let root = PathBuf::from("runtime-root");

    assert_eq!(
        java_executable_for_os(&root, "linux"),
        root.join("bin").join("java")
    );
    assert_eq!(
        java_executable_for_os(&root, "windows"),
        root.join("bin").join("javaw.exe")
    );
    assert_eq!(
        java_executable_for_os(&root, "macos"),
        root.join("jre.bundle")
            .join("Contents")
            .join("Home")
            .join("bin")
            .join("java")
    );
}

#[cfg(unix)]
#[test]
fn runtime_with_non_executable_java_is_broken() {
    let root = unique_temp_root("axial-runtime-non-executable-test");
    let java = java_executable(&root);
    fs::create_dir_all(java.parent().expect("java parent")).expect("java parent dir");
    fs::write(&java, b"java").expect("java file");
    fs::write(root.join(".axial-ready"), b"ready").expect("ready marker");

    assert_eq!(detect_runtime_state(&root), RuntimeInstallState::Broken);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_download_verification_accepts_matching_metadata() {
    let result = verify_runtime_download(
        "bin/java",
        &expected(Some(5), Some("AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D")),
        &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
    );

    assert_eq!(result, Ok(()));
}

#[test]
fn runtime_download_verification_rejects_size_mismatch() {
    let result = verify_runtime_download(
        "bin/java",
        &expected(Some(6), None),
        &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
    );

    assert_eq!(
        result,
        Err(RuntimeDownloadIntegrityError::SizeMismatch {
            file: "bin/java".to_string(),
            expected: 6,
            actual: 5,
        })
    );
}

#[test]
fn runtime_download_verification_rejects_sha1_mismatch() {
    let result = verify_runtime_download(
        "bin/java",
        &expected(None, Some("0000000000000000000000000000000000000000")),
        &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
    );

    assert_eq!(
        result,
        Err(RuntimeDownloadIntegrityError::Sha1Mismatch {
            file: "bin/java".to_string(),
            expected: "0000000000000000000000000000000000000000".to_string(),
            actual: "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string(),
        })
    );
}

#[test]
fn runtime_download_verification_accepts_missing_metadata() {
    let result = verify_runtime_download(
        "bin/java",
        &expected(None, None),
        &actual(5, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"),
    );

    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn runtime_file_download_streams_and_verifies_to_temp() {
    let root = unique_temp_root("axial-runtime-download-stream-test");
    fs::create_dir_all(&root).expect("download root");
    let temp_path = root.join("java.axial-tmp");
    let url = serve_runtime_download(b"hello".to_vec()).await;
    let client = runtime_download_client();

    fetch_runtime_file(
        &client,
        &url,
        &temp_path,
        expected(Some(5), Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")),
        "bin/java",
    )
    .await
    .expect("runtime download");

    assert_eq!(fs::read(&temp_path).expect("downloaded file"), b"hello");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ephemeral_processor_runtime_rejects_oversized_file_before_request() {
    let (url, requests) = serve_runtime_retry_responses(vec![(200, b"unused".to_vec())]).await;
    let manifest = ComponentManifest {
        files: HashMap::from([(
            "bin/java".to_string(),
            downloadable_manifest_file(
                &url,
                (128 << 20) + 1,
                "0000000000000000000000000000000000000000",
            ),
        )]),
    };

    assert!(validate_ephemeral_processor_manifest_for_test(&manifest, 1).is_err());
    tokio::task::yield_now().await;
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[test]
fn ephemeral_processor_runtime_counts_lzma_peak_entry_before_effects() {
    let mut files = (0..4091)
        .map(|index| (format!("entry-{index}"), manifest_file("directory")))
        .collect::<HashMap<_, _>>();
    files.insert(
        "bin/java".to_string(),
        downloadable_lzma_manifest_file(
            "https://example.invalid/java",
            1,
            "0000000000000000000000000000000000000000",
            "https://example.invalid/java.lzma",
            1,
            "0000000000000000000000000000000000000000",
        ),
    );
    let manifest = ComponentManifest { files };

    assert!(validate_ephemeral_processor_manifest_for_test(&manifest, 1).is_err());
}

#[tokio::test]
async fn runtime_file_download_retries_transient_status_errors() {
    let root = unique_temp_root("axial-runtime-download-retry-test");
    fs::create_dir_all(&root).expect("download root");
    let temp_path = root.join("java.axial-tmp");
    let (url, attempts) = serve_runtime_retry_responses(vec![
        (503, b"try again".to_vec()),
        (503, b"try again".to_vec()),
        (200, b"hello".to_vec()),
    ])
    .await;
    let client = runtime_download_client();

    fetch_runtime_file(
        &client,
        &url,
        &temp_path,
        expected(Some(5), Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")),
        "bin/java",
    )
    .await
    .expect("runtime download should retry transient failures");

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        fs::read(&temp_path).expect("retried runtime file"),
        b"hello"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_file_download_removes_temp_on_verification_error() {
    let root = unique_temp_root("axial-runtime-download-cleanup-test");
    fs::create_dir_all(&root).expect("download root");
    let temp_path = root.join("java.axial-tmp");
    let url = serve_runtime_download(b"hello".to_vec()).await;
    let client = runtime_download_client();

    let result = fetch_runtime_file(
        &client,
        &url,
        &temp_path,
        expected(Some(6), None),
        "bin/java",
    )
    .await;

    assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
    assert!(!temp_path.exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_file_download_rejects_oversized_content_length() {
    let root = unique_temp_root("axial-runtime-download-content-length-test");
    fs::create_dir_all(&root).expect("download root");
    let temp_path = root.join("java.axial-tmp");
    let url = serve_runtime_response(200, b"hello".to_vec(), Some(6), "/runtime.bin").await;
    let client = runtime_download_client();

    let result = fetch_runtime_file(
        &client,
        &url,
        &temp_path,
        expected(Some(5), None),
        "bin/java",
    )
    .await;

    assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
    assert!(!temp_path.exists());
    assert!(
        result
            .expect_err("oversized content length should fail")
            .to_string()
            .contains("runtime file bin/java size mismatch: expected 5, got 6")
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_file_requires_checksum_proof() {
    let root = unique_temp_root("axial-runtime-missing-checksum-test");
    let file = ComponentManifestFile {
        kind: "file".to_string(),
        executable: false,
        downloads: Some(ComponentManifestDownloads {
            raw: Some(ComponentManifestDownload {
                url: "https://example.test/runtime.bin".to_string(),
                sha1: None,
                size: Some(4),
            }),
            lzma: None,
        }),
        target: None,
    };

    let result =
        install_runtime_manifest_file(runtime_download_client().clone(), &root, "bin/java", file)
            .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::Download(message))
            if message.contains("missing checksum proof")
    ));
    assert!(!root.join("bin").join("java").exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_file_prefers_lzma_and_verifies_decompressed_output() {
    let root = unique_temp_root("axial-runtime-lzma-test");
    let raw_bytes = b"decompressed runtime file".to_vec();
    let compressed_bytes = lzma_compress_bytes(&raw_bytes);
    let lzma_url = serve_runtime_download(compressed_bytes.clone()).await;
    let file = downloadable_lzma_manifest_file(
        "http://127.0.0.1:9/raw-runtime-file",
        raw_bytes.len() as u64,
        &sha1_hex(&raw_bytes),
        &lzma_url,
        compressed_bytes.len() as u64,
        &sha1_hex(&compressed_bytes),
    );

    install_runtime_manifest_file(runtime_download_client().clone(), &root, "bin/java", file)
        .await
        .expect("runtime lzma file install");

    assert_eq!(
        fs::read(root.join("bin").join("java")).expect("decompressed runtime file"),
        raw_bytes
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_manifest_file_rejects_invalid_checksum_proof() {
    let root = unique_temp_root("axial-runtime-invalid-checksum-test");
    let file = ComponentManifestFile {
        kind: "file".to_string(),
        executable: false,
        downloads: Some(ComponentManifestDownloads {
            raw: Some(ComponentManifestDownload {
                url: "https://example.test/runtime.bin".to_string(),
                sha1: Some("not-a-sha1".to_string()),
                size: Some(4),
            }),
            lzma: None,
        }),
        target: None,
    };

    let result =
        install_runtime_manifest_file(runtime_download_client().clone(), &root, "bin/java", file)
            .await;

    assert!(matches!(
        result,
        Err(JavaRuntimeLookupError::Download(message))
            if message.contains("missing checksum proof")
    ));
    assert!(!root.join("bin").join("java").exists());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_file_download_rejects_stream_past_expected_size_and_removes_temp() {
    let root = unique_temp_root("axial-runtime-download-stream-bound-test");
    fs::create_dir_all(&root).expect("download root");
    let temp_path = root.join("java.axial-tmp");
    let url = serve_runtime_response(200, b"hello!".to_vec(), None, "/runtime.bin").await;
    let client = runtime_download_client();

    let result = fetch_runtime_file(
        &client,
        &url,
        &temp_path,
        expected(Some(5), None),
        "bin/java",
    )
    .await;

    assert!(matches!(&result, Err(JavaRuntimeLookupError::Download(_))));
    assert!(!temp_path.exists());
    assert!(
        result
            .expect_err("oversized stream should fail")
            .to_string()
            .contains("runtime file bin/java size mismatch: expected 5, got 6")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_install_futures_stay_small_enough_for_tokio_workers() {
    let root = Path::new("/tmp/axial-runtime-future-size");
    let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
    let client = runtime_download_client();
    let expected = expected(Some(8), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    let file = downloadable_manifest_file(
        "https://example.test/runtime.bin",
        8,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let spawned_client = client.clone();
    let spawned_root = root.to_path_buf();
    let spawned_file = file.clone();
    let spawned_future = async move {
        Box::pin(install_runtime_manifest_file(
            spawned_client,
            &spawned_root,
            "bin/java",
            spawned_file,
        ))
        .await
    };

    assert!(
        std::mem::size_of_val(&fetch_runtime_file(
            &client,
            "https://example.test/runtime.bin",
            &root.join("java.axial-tmp"),
            expected,
            "bin/java",
        )) < 4096,
        "runtime file download future should stay small"
    );
    assert!(
        std::mem::size_of_val(&install_runtime_manifest_file(
            client.clone(),
            root,
            "bin/java",
            file.clone(),
        )) < 4096,
        "runtime manifest file install future should stay small"
    );
    assert!(
        std::mem::size_of_val(&spawned_future) < 4096,
        "spawned runtime manifest file install future should stay small"
    );
    assert!(
        std::mem::size_of_val(&ensure_runtime_with_events(
            &runtime_cache,
            &JavaVersion {
                component: "java-runtime-delta".to_string(),
                major_version: 21,
            },
            "",
            false,
            None,
            |_| {},
        )) < 4096,
        "managed-runtime ensure future should stay small"
    );
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

fn runtime_manifest_fixture(value: serde_json::Value) -> RuntimeManifest {
    serde_json::from_value(value).expect("runtime manifest fixture")
}

fn write_runtime_executable_fixture(root: &Path) {
    let java = java_executable(root);
    fs::create_dir_all(java.parent().expect("java parent")).expect("java parent dir");
    fs::write(&java, b"java").expect("java executable");
    make_executable(&java);
    if cfg!(target_os = "windows") {
        let config = root.join("lib").join("jvm.cfg");
        fs::create_dir_all(config.parent().expect("config parent")).expect("config parent dir");
        fs::write(config, b"jvm").expect("runtime config");
    }
}

fn write_runtime_manifest_proof_for_java(root: &Path) {
    let java = java_executable(root);
    let bytes = fs::read(&java).expect("read java fixture");
    let relative_path = java
        .strip_prefix(root)
        .expect("java under root")
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
        root.join(".axial-runtime-manifest.json"),
        serde_json::to_vec(&manifest).expect("manifest json"),
    )
    .expect("write runtime manifest proof");
}

#[cfg(unix)]
fn write_runtime_manifest_proof_for_java_and_link(root: &Path) {
    let java = java_executable(root);
    let link = java.with_file_name("java-link");
    let bytes = fs::read(&java).expect("read java fixture");
    let relative_path = java
        .strip_prefix(root)
        .expect("java under root")
        .to_string_lossy()
        .replace('\\', "/");
    let link_relative_path = link
        .strip_prefix(root)
        .expect("link under root")
        .to_string_lossy()
        .replace('\\', "/");
    let manifest = serde_json::json!({
        "files": {
            relative_path: {
                "type": "file",
                "downloads": {
                    "raw": {
                        "url": "https://example.invalid/java",
                        "sha1": sha1_hex(&bytes),
                        "size": bytes.len()
                    }
                }
            },
            link_relative_path: {
                "type": "link",
                "target": "java"
            }
        }
    });
    fs::write(
        root.join(".axial-runtime-manifest.json"),
        serde_json::to_vec(&manifest).expect("manifest json"),
    )
    .expect("write runtime manifest proof with link");
}

fn lzma_compress_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut input = std::io::Cursor::new(bytes);
    let mut output = Vec::new();
    lzma_rs::lzma_compress(&mut input, &mut output).expect("compress lzma fixture");
    output
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

async fn serve_runtime_download(body: Vec<u8>) -> String {
    let content_length = body.len() as u64;
    serve_runtime_response(200, body, Some(content_length), "/runtime.bin").await
}

async fn serve_runtime_json(status: u16, body: Vec<u8>, content_length: Option<u64>) -> String {
    let content_length = content_length.unwrap_or(body.len() as u64);
    serve_runtime_response(status, body, Some(content_length), "/runtime.json").await
}

async fn serve_runtime_response(
    status: u16,
    body: Vec<u8>,
    content_length: Option<u64>,
    path: &str,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("runtime test listener");
    let address = listener
        .local_addr()
        .expect("runtime test listener address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("runtime test connection");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        let reason = if status == 200 { "OK" } else { "Error" };
        let headers = if let Some(content_length) = content_length {
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            )
        } else {
            format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n\r\n")
        };
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("runtime test response headers");
        socket
            .write_all(&body)
            .await
            .expect("runtime test response body");
    });
    format!("http://{address}{path}")
}

async fn serve_runtime_retry_responses(
    responses: Vec<(u16, Vec<u8>)>,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("runtime retry test listener");
    let address = listener
        .local_addr()
        .expect("runtime retry test listener address");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_in_task = Arc::clone(&attempts);
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("runtime retry test connection");
            attempts_in_task.fetch_add(1, Ordering::SeqCst);
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("runtime retry response headers");
            socket
                .write_all(&body)
                .await
                .expect("runtime retry response body");
        }
    });
    (format!("http://{address}/runtime.bin"), attempts)
}
